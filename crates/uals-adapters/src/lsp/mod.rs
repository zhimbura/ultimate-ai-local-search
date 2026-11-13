//! Precise usages from language servers.
//!
//! [`LspPool`] runs one server per language and project: the project is the nearest folder with
//! a build file of that language (`settings.gradle.kts`, `go.mod`, `Cargo.toml`, ...). A folder
//! with a Go module and an Android project therefore runs gopls, kotlin-lsp and jdtls side by
//! side, each on its own project.
//!
//! A server starts on the first `usages` call that needs it and stops after `idle_minutes`
//! without calls. A call waits at most `ready_wait_ms` for a server that is still loading its
//! project; then it answers `Unavailable` and the usages stay by name until a later call.

mod client;
mod languages;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uals_core::backend::{BackendError, ReferenceResolver};
use uals_core::config::{LspConfig, LspServerConfig};
use uals_core::types::{Definition, Reference, Repo, ResolvedReferences};

use self::client::{Launch, LspClient};
use self::languages::{Readiness, STATE_DIR, ServerSpec};
use crate::mcp_child::{ChildSpec, resolve_program};

/// A server that failed to start or to load its project is not started again before this.
const FAILED_COOLDOWN: Duration = Duration::from_secs(300);
/// Quiet time after the last progress before a server counts as ready.
const SETTLE: Duration = Duration::from_secs(2);
/// A server that reports loading as progress but sent none is ready after this.
const LOAD_GRACE: Duration = Duration::from_secs(15);
/// How often readiness is checked while a call waits.
const READY_POLL: Duration = Duration::from_millis(100);
/// How often idle servers are looked for.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Starts language servers on demand and asks them for references.
pub struct LspPool {
    inner: Arc<Inner>,
}

struct Inner {
    config: LspConfig,
    /// Parent of the per-project state folders of the servers that need one.
    state_root: PathBuf,
    slots: StdMutex<HashMap<Key, Arc<Slot>>>,
    sweeper_started: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    server: &'static str,
    root: PathBuf,
}

/// One server for one project, started or not.
struct Slot {
    spec: &'static ServerSpec,
    root: PathBuf,
    state: Mutex<SlotState>,
    last_used: StdMutex<Instant>,
    /// Set once the server was ready; later calls do not wait for background progress.
    ready: AtomicBool,
}

enum SlotState {
    Stopped,
    Running(Arc<LspClient>),
    Failed { reason: String, until: Instant },
}

impl LspPool {
    /// `state_root` is where servers that need a state folder (kotlin-lsp, jdtls) keep one per
    /// project.
    pub fn new(config: LspConfig, state_root: PathBuf) -> Self {
        for name in config.servers.keys().filter(|name| languages::server_named(name).is_none()) {
            tracing::warn!(server = %name, "lsp.servers names a server uals does not know; it is ignored");
        }
        Self {
            inner: Arc::new(Inner {
                config,
                state_root,
                slots: StdMutex::default(),
                sweeper_started: AtomicBool::new(false),
            }),
        }
    }

    /// Stops every server.
    pub async fn shutdown(&self) {
        let slots: Vec<Arc<Slot>> = self.inner.slots().values().cloned().collect();
        for slot in slots {
            slot.stop().await;
        }
    }
}

impl Inner {
    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Arc<Slot>>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn server_config(&self, spec: &ServerSpec) -> LspServerConfig {
        self.config.servers.get(spec.name).cloned().unwrap_or_default()
    }

    fn slot(&self, spec: &'static ServerSpec, root: &Path) -> Arc<Slot> {
        let key = Key { server: spec.name, root: root.to_path_buf() };
        let mut slots = self.slots();
        Arc::clone(slots.entry(key).or_insert_with(|| {
            Arc::new(Slot {
                spec,
                root: root.to_path_buf(),
                state: Mutex::new(SlotState::Stopped),
                last_used: StdMutex::new(Instant::now()),
                ready: AtomicBool::new(false),
            })
        }))
    }

    /// The running server of `slot`, started when there is none.
    async fn client(self: &Arc<Self>, slot: &Slot) -> Result<Arc<LspClient>, BackendError> {
        *slot.last_used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        let mut state = slot.state.lock().await;
        match &*state {
            SlotState::Running(client) if client.is_alive() => return Ok(Arc::clone(client)),
            SlotState::Running(client) => {
                let reason = format!("{} exited{}", slot.spec.name, client.stderr());
                *state = SlotState::Failed { reason: reason.clone(), until: Instant::now() + FAILED_COOLDOWN };
                return Err(BackendError::Unavailable(reason));
            }
            SlotState::Failed { reason, until } if *until > Instant::now() => {
                return Err(BackendError::Unavailable(reason.clone()));
            }
            SlotState::Failed { .. } | SlotState::Stopped => {}
        }
        slot.ready.store(false, Ordering::SeqCst);
        let launch = self.launch(slot)?;
        match LspClient::start(launch, Duration::from_millis(self.config.request_ms)).await {
            Ok(client) => {
                *state = SlotState::Running(Arc::clone(&client));
                self.start_sweeper();
                Ok(client)
            }
            Err(error) => {
                let reason = error_text(&error);
                *state = SlotState::Failed { reason, until: Instant::now() + FAILED_COOLDOWN };
                Err(error)
            }
        }
    }

    /// How to start the server of `slot`: the first installed command, with the state folder of
    /// its project in place of [`STATE_DIR`].
    fn launch(&self, slot: &Slot) -> Result<Launch, BackendError> {
        let spec = slot.spec;
        let (mut child, _) = installed_command(spec, &self.server_config(spec), Some(slot.root.clone()))?;
        let mut state_lock = None;
        if child.args.iter().any(|arg| arg.contains(STATE_DIR)) {
            let (dir, lock) = state_dir(&self.state_root, spec.name, &slot.root)?;
            let dir = dir.display().to_string();
            for arg in &mut child.args {
                *arg = arg.replace(STATE_DIR, &dir);
            }
            state_lock = Some(lock);
        }
        Ok(Launch {
            child,
            load_failures: spec.load_failures.iter().map(|text| (*text).to_owned()).collect(),
            root_uri: file_uri(&slot.root)?,
            root_name: folder_label(&slot.root),
            state_lock,
        })
    }

    /// Starts the task that stops servers idle for longer than `idle_minutes`, once.
    fn start_sweeper(self: &Arc<Self>) {
        if self.sweeper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let idle = Duration::from_secs(self.config.idle_minutes.saturating_mul(60));
        let weak: Weak<Self> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_EVERY.min(idle.max(Duration::from_secs(1)))).await;
                let Some(inner) = weak.upgrade() else { return };
                let slots: Vec<Arc<Slot>> = inner.slots().values().cloned().collect();
                drop(inner);
                for slot in slots {
                    slot.stop_if_idle(idle).await;
                }
            }
        });
    }
}

impl Slot {
    /// Stops the server when it runs. A failed slot keeps its failure.
    async fn stop(&self) {
        let mut state = self.state.lock().await;
        if let SlotState::Running(client) = &*state {
            tracing::debug!(server = %self.spec.name, root = %self.root.display(), "stopping language server");
            client.shutdown().await;
            *state = SlotState::Stopped;
        }
    }

    /// Stops the server when it was not asked for `idle`. A call takes the state lock after it
    /// marks the slot used, so a server a call is about to use is never stopped.
    async fn stop_if_idle(&self, idle: Duration) {
        let mut state = self.state.lock().await;
        let last_used = *self.last_used.lock().unwrap_or_else(PoisonError::into_inner);
        if let SlotState::Running(client) = &*state
            && last_used.elapsed() >= idle
        {
            tracing::debug!(server = %self.spec.name, root = %self.root.display(), "stopping idle language server");
            client.shutdown().await;
            *state = SlotState::Stopped;
        }
    }

    /// Marks the slot failed and stops its server.
    async fn fail(&self, reason: String) {
        let mut state = self.state.lock().await;
        if let SlotState::Running(client) = &*state {
            client.shutdown().await;
        }
        *state = SlotState::Failed { reason, until: Instant::now() + FAILED_COOLDOWN };
    }

    /// One line for `status`.
    fn describe(&self) -> String {
        let head = format!("{} {}", self.spec.name, self.root.display());
        let Ok(state) = self.state.try_lock() else { return format!("{head}: starting") };
        match &*state {
            SlotState::Stopped => format!("{head}: stopped (idle)"),
            SlotState::Failed { reason, .. } => format!("{head}: failed: {reason}"),
            SlotState::Running(_) if self.ready.load(Ordering::SeqCst) => format!("{head}: ready"),
            SlotState::Running(client) => match readiness(self.spec, client) {
                Ok(true) => format!("{head}: ready"),
                Ok(false) => format!("{head}: loading ({}s)", client.started().elapsed().as_secs()),
                Err(reason) => format!("{head}: failed: {reason}"),
            },
        }
    }
}

/// Waits up to `wait` for the server of `slot` to load its project, once per server. Marks the slot
/// failed when the project cannot load.
async fn wait_until_ready(slot: &Slot, client: &LspClient, wait: Duration) -> Result<(), BackendError> {
    if slot.ready.load(Ordering::SeqCst) {
        return Ok(());
    }
    let spec = slot.spec;
    let deadline = Instant::now() + wait;
    loop {
        match readiness(spec, client) {
            Ok(true) => break,
            Ok(false) if Instant::now() < deadline => tokio::time::sleep(READY_POLL).await,
            Ok(false) => {
                return Err(BackendError::Unavailable(format!(
                    "{} is still loading {} ({}s so far); usages are by name until it is ready",
                    spec.name,
                    slot.root.display(),
                    client.started().elapsed().as_secs()
                )));
            }
            Err(reason) => {
                slot.fail(reason.clone()).await;
                return Err(BackendError::Unavailable(reason));
            }
        }
    }
    slot.ready.store(true, Ordering::SeqCst);
    Ok(())
}

/// Whether `client` can answer: `Err` when it cannot load its project at all.
fn readiness(spec: &ServerSpec, client: &LspClient) -> Result<bool, String> {
    let started = client.started();
    client.with_state(|state| {
        if state.exited {
            return Err(format!("{} exited{}", spec.name, client.stderr()));
        }
        if let Some(failure) = &state.load_failure {
            return Err(format!("{} could not load the project: {failure}", spec.name));
        }
        let settled = state.active_progress.is_empty() && state.last_progress.elapsed() >= SETTLE;
        Ok(match spec.readiness {
            // A rust-analyzer too old to send its status is judged by progress after the grace.
            Readiness::QuiescentStatus => match state.quiescent {
                Some(quiescent) => quiescent,
                None => settled && started.elapsed() >= LOAD_GRACE,
            },
            Readiness::Progress { reports_loading } => {
                settled && (!reports_loading || state.seen_progress || started.elapsed() >= LOAD_GRACE)
            }
        })
    })
}

#[async_trait]
impl ReferenceResolver for LspPool {
    async fn references(&self, repo: &Repo, definition: &Definition) -> Result<ResolvedReferences, BackendError> {
        let inner = &self.inner;
        let spec = languages::server_for(&definition.path).ok_or_else(|| {
            BackendError::Unavailable(format!("no language server handles {}", definition.path.display()))
        })?;
        if !inner.config.enabled || !inner.server_config(spec).enabled {
            return Err(BackendError::Unavailable(format!("{} is turned off in the config", spec.name)));
        }
        let boundary = canonical(&repo.root);
        let path = canonical(&definition.path);
        let root = languages::project_root(spec, &path, &boundary);
        let slot = inner.slot(spec, &root);
        let client = inner.client(&slot).await?;

        let text = tokio::fs::read(&path)
            .await
            .map_err(|e| BackendError::Unavailable(format!("cannot read {}: {e}", path.display())))?;
        let text = String::from_utf8_lossy(&text).into_owned();
        let line_index = definition.line.saturating_sub(1);
        let column = text
            .lines()
            .nth(line_index as usize)
            .and_then(|line| languages::identifier_column(line, &definition.name))
            .ok_or_else(|| {
                BackendError::Unavailable(format!(
                    "'{}' is not on line {} of {} (the index may be older than the file)",
                    definition.name,
                    definition.line,
                    path.display()
                ))
            })?;
        let uri = file_uri(&path)?;
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line_index, "character": column },
            "context": { "includeDeclaration": false }
        });
        let timeout = Duration::from_millis(inner.config.request_ms);
        let answer = client
            .with_document(
                &uri,
                languages::language_id(&path),
                || text,
                // Some servers (typescript-language-server) load the project of a file only when
                // it is opened, so readiness is awaited with the file open.
                async || {
                    wait_until_ready(&slot, &client, Duration::from_millis(inner.config.ready_wait_ms)).await?;
                    client.request("textDocument/references", params, timeout).await
                },
            )
            .await?;
        *slot.last_used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();

        let references = parse_locations(&answer, &[(&boundary, &repo.root)]);
        if references.is_empty() && spec.empty_is_unknown {
            return Err(BackendError::Unavailable(format!(
                "{} found no usages; its index of {} may not be built",
                spec.name,
                root.display()
            )));
        }
        Ok(ResolvedReferences {
            server: spec.name.to_owned(),
            root: root.clone(),
            extensions: spec.extensions.iter().map(|e| (*e).to_owned()).collect(),
            references,
        })
    }

    fn servers(&self) -> Vec<String> {
        let mut slots: Vec<Arc<Slot>> = self.inner.slots().values().cloned().collect();
        slots.sort_by(|a, b| (a.spec.name, &a.root).cmp(&(b.spec.name, &b.root)));
        slots.iter().map(|slot| slot.describe()).collect()
    }
}

/// Where each known server's program is, or why it is not used: for `uals doctor`.
pub fn locate(config: &LspConfig) -> Vec<(&'static str, Result<PathBuf, String>)> {
    languages::SERVERS
        .iter()
        .map(|spec| {
            let server = config.servers.get(spec.name).cloned().unwrap_or_default();
            if !server.enabled {
                return (spec.name, Err("turned off in the config".to_owned()));
            }
            (spec.name, installed_command(spec, &server, None).map(|(_, program)| program).map_err(|e| error_text(&e)))
        })
        .collect()
}

/// The first command of `spec` (or the one `config` sets) whose program is installed, with the
/// server's environment, and the program's path. The environment holds `JAVA_HOME` for the JVM
/// servers: `config.java_home`, else the one of uals.
fn installed_command(
    spec: &ServerSpec,
    config: &LspServerConfig,
    cwd: Option<PathBuf>,
) -> Result<(ChildSpec, PathBuf), BackendError> {
    let mut env = config.env.clone();
    let mut pass_env = config.pass_env.clone();
    if spec.needs_java {
        match &config.java_home {
            Some(java_home) => {
                env.insert("JAVA_HOME".into(), java_home.display().to_string());
            }
            None => pass_env.push("JAVA_HOME".into()),
        }
    }
    let commands: Vec<Vec<String>> = if config.command.is_empty() {
        spec.commands.iter().map(|command| command.iter().map(|arg| (*arg).to_owned()).collect()).collect()
    } else {
        vec![config.command.clone()]
    };
    let mut tried = Vec::new();
    for command in commands {
        let Some((program, args)) = command.split_first() else { continue };
        let child = ChildSpec {
            name: spec.name.to_owned(),
            program: PathBuf::from(program),
            args: args.to_vec(),
            env: env.clone(),
            pass_env: pass_env.clone(),
            cwd: cwd.clone(),
        };
        match resolve_program(&child, &child.child_env()) {
            Ok(path) => return Ok((child, path)),
            Err(_) => tried.push(program.clone()),
        }
    }
    Err(BackendError::Unavailable(format!("{} is not installed (looked for {})", spec.name, tried.join(" or "))))
}

/// The last component of `folder`, for names and labels; `root` for `/`.
fn folder_label(folder: &Path) -> String {
    folder.file_name().map_or_else(|| "root".into(), |name| name.to_string_lossy().into_owned())
}

/// The `file://` locations of a `textDocument/references` answer, sorted and without repeats.
/// `prefixes` rewrite a path that starts with the first path of a pair to start with the second.
fn parse_locations(answer: &Value, prefixes: &[(&Path, &Path)]) -> Vec<Reference> {
    let mut references: Vec<Reference> = answer
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|location| {
            let uri = location.get("uri").and_then(Value::as_str)?;
            let line = location.pointer("/range/start/line").and_then(Value::as_u64)?;
            let path = url::Url::parse(uri).ok().filter(|u| u.scheme() == "file")?.to_file_path().ok()?;
            let path = prefixes
                .iter()
                .find_map(|(from, to)| path.strip_prefix(from).ok().map(|rest| to.join(rest)))
                .unwrap_or(path);
            Some(Reference { path, line: u32::try_from(line + 1).ok()? })
        })
        .collect();
    references.sort();
    references.dedup();
    references
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn file_uri(path: &Path) -> Result<String, BackendError> {
    url::Url::from_file_path(path)
        .map(String::from)
        .map_err(|()| BackendError::Failed(format!("not an absolute path: {}", path.display())))
}

fn error_text(error: &BackendError) -> String {
    match error {
        BackendError::Unavailable(text) | BackendError::Failed(text) => text.clone(),
    }
}

/// The state folder of `server` for the project `root`, and the lock that reserves it. When
/// another uals process holds the shared folder, a folder of this process is used: two servers
/// must not share one.
fn state_dir(state_root: &Path, server: &str, root: &Path) -> Result<(PathBuf, std::fs::File), BackendError> {
    let base = folder_label(root);
    let shared = state_root.join(server).join(format!("{base}-{:016x}", fnv1a(root.to_string_lossy().as_bytes())));
    let own = PathBuf::from(format!("{}-{}", shared.display(), std::process::id()));
    for dir in [shared, own] {
        let fail = |e: std::io::Error| BackendError::Unavailable(format!("cannot create {}: {e}", dir.display()));
        std::fs::create_dir_all(&dir).map_err(fail)?;
        let lock = std::fs::File::create(format!("{}.lock", dir.display())).map_err(fail)?;
        if lock.try_lock().is_ok() {
            return Ok((dir, lock));
        }
    }
    Err(BackendError::Unavailable(format!("the state folder of {server} for {} is locked", root.display())))
}

/// FNV-1a: a hash that stays the same across Rust versions, for folder names.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_map_to_one_based_lines_under_the_given_roots() {
        let answer = json!([
            { "uri": "file:///private/repo/a/B.kt", "range": { "start": { "line": 9, "character": 4 } } },
            { "uri": "file:///private/repo/a/B.kt", "range": { "start": { "line": 9, "character": 20 } } },
            { "uri": "file:///elsewhere/C.kt", "range": { "start": { "line": 0, "character": 0 } } },
            { "uri": "jar:file:///x.jar!/D.class", "range": { "start": { "line": 1, "character": 0 } } }
        ]);
        let references = parse_locations(&answer, &[(Path::new("/private/repo"), Path::new("/repo"))]);
        assert_eq!(
            references,
            vec![
                Reference { path: PathBuf::from("/elsewhere/C.kt"), line: 1 },
                Reference { path: PathBuf::from("/repo/a/B.kt"), line: 10 },
            ]
        );
        assert!(parse_locations(&Value::Null, &[]).is_empty());
    }

    #[test]
    fn a_locked_state_folder_is_not_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/work/app");
        let (first, _lock) = state_dir(tmp.path(), "kotlin-lsp", root).unwrap();
        let (second, _second_lock) = state_dir(tmp.path(), "kotlin-lsp", root).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(tmp.path().join("kotlin-lsp")));
        assert!(first.file_name().unwrap().to_string_lossy().starts_with("app-"));
    }
}
