//! The MCP server: the tools `search`, `usages` and `status` on stdin and stdout.
//!
//! - The folder of a call is its `workspace` parameter, else the client's first local root folder
//!   (when the client offers roots and lists them within 2 s), else `$UALS_WORKSPACE`, else the
//!   folder the server runs in.
//! - The first call in a workspace prepares it in the background: the backends start (semantic
//!   servers, background indexing) and, when `ast_index.update_on_start` is set, the existing
//!   ast-index indexes are updated. The workspace of `$UALS_WORKSPACE` or the server's folder is
//!   prepared at start, before the first call. A workspace with more than `MAX_PREPARED_REPOS`
//!   repositories is not prepared: that would start as many indexing jobs at once; its backends
//!   start on first use.
//! - The server stops when the client closes the connection, or on SIGTERM or SIGINT. Then the
//!   runtime drops every task and with them the orchestrator, which stops the backend servers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{debug, info, warn};
use uals_core::config::Config;
use uals_core::render::{render_search, render_usages};
use uals_core::types::{Repo, Workspace};

use crate::args::{SearchArgs, StatusArgs, UsagesArgs, repos_in, workspace_folder};
use crate::orchestrator::Orchestrator;
use crate::report::{now_ms, render_status};
use crate::setup;

/// A workspace with more repositories than this is not prepared in the background.
pub const MAX_PREPARED_REPOS: usize = 16;

/// How long a call waits for the client to list its roots before it uses the next source.
const ROOTS_DEADLINE: Duration = Duration::from_secs(2);

const INSTRUCTIONS: &str = "uals searches all repositories under one folder at once. Use `search` \
first for any question about the code, `usages` to list every reference to a symbol, and `status` \
when results look stale or a backend is missing. The folder is the `workspace` argument, else the \
client's root folder, else $UALS_WORKSPACE, else the folder the server runs in.";

const SEARCH_DESCRIPTION: &str = "Search code in every repository of a folder at once: symbol \
definitions (ast-index), text (ripgrep engine) and meaning (semantic index), fused into one ranked \
list with code snippets. Use it first for any question about the code: a symbol name, words that \
describe the code, a regular expression, or an exact string. Each hit names the repository, the \
absolute path and the lines; `backends:` says which sources answered; `next: offset=N` gives the \
next page.";

const USAGES_DESCRIPTION: &str = "List every usage of a symbol in every repository of a folder, \
grouped by repository and file, with line numbers. Uses the ast-index index where a repository \
has one, otherwise a whole-word text search; each repository says which source answered.";

const STATUS_DESCRIPTION: &str = "Show the state of each search backend's index in every \
repository of a folder: indexed or not, when it was updated, and why a backend is not available. \
Use it when results look stale or a backend is missing.";

/// The MCP tools over one orchestrator.
#[derive(Clone)]
pub struct UalsServer {
    orchestrator: Arc<Orchestrator>,
    /// Roots of the workspaces already prepared.
    prepared: Arc<Mutex<HashSet<PathBuf>>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl UalsServer {
    /// A server that answers with `orchestrator`.
    pub fn new(orchestrator: Arc<Orchestrator>) -> Self {
        Self { orchestrator, prepared: Arc::default(), tool_router: Self::tool_router() }
    }

    #[tool(description = SEARCH_DESCRIPTION)]
    async fn search(&self, Parameters(args): Parameters<SearchArgs>, peer: Peer<RoleServer>) -> Result<String, String> {
        let repos = self.repos(args.workspace.as_deref(), &args.repos, &peer).await?;
        let response = self.orchestrator.search(&repos, &args.params()).await.map_err(message)?;
        Ok(render_search(&response, args.format.into(), &self.orchestrator.config().output))
    }

    #[tool(description = USAGES_DESCRIPTION)]
    async fn usages(&self, Parameters(args): Parameters<UsagesArgs>, peer: Peer<RoleServer>) -> Result<String, String> {
        let repos = self.repos(args.workspace.as_deref(), &args.repos, &peer).await?;
        let response = self.orchestrator.usages(&repos, &args.params()).await.map_err(message)?;
        Ok(render_usages(&response, args.format.into(), &self.orchestrator.config().output))
    }

    #[tool(description = STATUS_DESCRIPTION)]
    async fn status(&self, Parameters(args): Parameters<StatusArgs>, peer: Peer<RoleServer>) -> Result<String, String> {
        let folder = self.folder(args.workspace.as_deref(), &peer).await?;
        let (workspace, repos) = repos_in(&folder, &args.repos, self.orchestrator.config()).map_err(message)?;
        self.prepare(&workspace);
        let states = self.orchestrator.status(&repos).await;
        Ok(render_status(&workspace.root, &repos, &states, args.format.into(), now_ms()))
    }
}

impl UalsServer {
    /// The folder of a call (see the module docs).
    async fn folder(&self, explicit: Option<&Path>, peer: &Peer<RoleServer>) -> Result<PathBuf, String> {
        let client_root = match explicit {
            Some(_) => None,
            None => client_root(peer).await,
        };
        workspace_folder(explicit, client_root.as_deref()).map_err(message)
    }

    /// The repositories of a call, named in `names` (all when empty). Prepares their workspace on
    /// its first call.
    async fn repos(
        &self,
        explicit: Option<&Path>,
        names: &[String],
        peer: &Peer<RoleServer>,
    ) -> Result<Vec<Repo>, String> {
        let folder = self.folder(explicit, peer).await?;
        let (workspace, repos) = repos_in(&folder, names, self.orchestrator.config()).map_err(message)?;
        self.prepare(&workspace);
        Ok(repos)
    }

    /// Prepares the workspace of `$UALS_WORKSPACE` or the current folder, if it has repositories.
    fn prepare_default_workspace(&self) {
        let found = workspace_folder(None, None).and_then(|folder| repos_in(&folder, &[], self.orchestrator.config()));
        match found {
            Ok((workspace, _)) => self.prepare(&workspace),
            Err(error) => debug!(error = %format!("{error:#}"), "no workspace to prepare at start"),
        }
    }

    /// Starts the backends for `workspace` in the background, once per workspace (see the module
    /// docs).
    fn prepare(&self, workspace: &Workspace) {
        let first = self.prepared.lock().unwrap_or_else(PoisonError::into_inner).insert(workspace.root.clone());
        if !first {
            return;
        }
        let root = workspace.root.display();
        if workspace.repos.len() > MAX_PREPARED_REPOS {
            warn!(workspace = %root, repos = workspace.repos.len(), limit = MAX_PREPARED_REPOS, "too many repositories to prepare; backends start on first use");
            return;
        }
        info!(workspace = %root, repos = workspace.repos.len(), "preparing the workspace");
        let orchestrator = Arc::clone(&self.orchestrator);
        let repos = workspace.repos.clone();
        tokio::spawn(async move {
            orchestrator.warm(&repos).await;
            if orchestrator.config().ast_index.update_on_start {
                // Dropping the handle leaves the updates running.
                drop(orchestrator.update_existing_ast_indexes_in_background(&repos));
            }
        });
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for UalsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("uals", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serves the tools on stdin and stdout until the client closes the connection or the process
/// gets SIGTERM or SIGINT.
pub async fn serve(config: Config) -> Result<()> {
    let server = UalsServer::new(Arc::new(setup::orchestrator(config)));
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let running = server.clone().serve(rmcp::transport::stdio()).await?;
    server.prepare_default_workspace();
    tokio::select! {
        quit = running.waiting() => debug!(?quit, "the client closed the connection"),
        _ = terminate.recv() => info!("SIGTERM: stopping"),
        _ = interrupt.recv() => info!("SIGINT: stopping"),
    }
    Ok(())
}

/// The first local folder among the client's roots, when the client offers roots and lists them
/// within `ROOTS_DEADLINE`.
#[expect(
    deprecated,
    reason = "roots are deprecated by SEP-2577; a client that does not list them sends the call on to the next source"
)]
async fn client_root(peer: &Peer<RoleServer>) -> Option<PathBuf> {
    let offers_roots = peer.peer_info().is_some_and(|info| info.capabilities.roots.is_some());
    if !offers_roots {
        return None;
    }
    match tokio::time::timeout(ROOTS_DEADLINE, peer.list_roots()).await {
        Ok(Ok(listed)) => listed.roots.iter().find_map(|root| file_uri_path(&root.uri)),
        Ok(Err(error)) => {
            debug!(%error, "the client did not list its roots");
            None
        }
        Err(_) => {
            debug!(deadline = ?ROOTS_DEADLINE, "the client did not list its roots in time");
            None
        }
    }
}

/// The local path of a `file://` URI: `file:///srv/My%20Code` gives `/srv/My Code`.
/// `None` for other schemes, other hosts, bad escapes and paths that are not UTF-8.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = path.get(at + 1..at + 3).filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(decoded).ok().map(PathBuf::from)
}

/// An error as the text a tool returns.
fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use pretty_assertions::assert_eq;
    use uals_core::backend::{BackendError, SearchBackend};
    use uals_core::types::{BackendAnswer, BackendId, IndexState, QueryKind, SearchRequest};

    use super::*;
    use crate::orchestrator::Backends;

    /// An indexed ast-index stand-in that counts `warm` and `refresh` calls.
    #[derive(Default)]
    struct Counting {
        warms: AtomicUsize,
        refreshes: AtomicUsize,
    }

    #[async_trait]
    impl SearchBackend for Counting {
        fn id(&self) -> BackendId {
            BackendId::AstIndex
        }

        fn supports(&self, _kind: QueryKind) -> bool {
            true
        }

        async fn search(&self, _repo: &Repo, _request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
            Ok(BackendAnswer { hits: Vec::new(), stale: None, partial: None })
        }

        async fn index_state(&self, repo: &Repo) -> IndexState {
            IndexState {
                backend: BackendId::AstIndex,
                repo: repo.name.clone(),
                indexed: true,
                updated_at_ms: None,
                stale: None,
                detail: None,
            }
        }

        async fn warm(&self, _repos: &[Repo]) -> Result<(), BackendError> {
            self.warms.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn refresh(&self, _repo: &Repo, _full: bool) -> Result<(), BackendError> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn workspace(repos: usize) -> Workspace {
        let repos = (0..repos).map(|n| Repo { name: format!("r{n}"), root: PathBuf::from(format!("/ws/r{n}")) });
        Workspace { root: PathBuf::from("/ws"), repos: repos.collect() }
    }

    /// Prepares `workspaces` one after another and returns how many times the backend was warmed
    /// and refreshed once the background work is over.
    async fn prepare_all(update_on_start: bool, workspaces: &[Workspace]) -> (usize, usize) {
        let backend = Arc::new(Counting::default());
        let mut config = Config::default();
        config.ast_index.update_on_start = update_on_start;
        let backends = Backends { search: vec![backend.clone()], usages: Vec::new(), spans: None };
        let server = UalsServer::new(Arc::new(Orchestrator::new(backends, config)));
        for workspace in workspaces {
            server.prepare(workspace);
        }
        // Paused time moves on only when every task waits, so the spawned work is over by then.
        tokio::time::sleep(Duration::from_secs(1)).await;
        (backend.warms.load(Ordering::SeqCst), backend.refreshes.load(Ordering::SeqCst))
    }

    #[tokio::test(start_paused = true)]
    async fn each_workspace_is_warmed_and_its_ast_indexes_updated_once() {
        let two = workspace(2);
        assert_eq!(prepare_all(true, &[two.clone(), two.clone()]).await, (1, 2));
        assert_eq!(prepare_all(false, &[two]).await, (1, 0));
        assert_eq!(prepare_all(true, &[workspace(MAX_PREPARED_REPOS)]).await, (1, MAX_PREPARED_REPOS));
        assert_eq!(prepare_all(true, &[workspace(MAX_PREPARED_REPOS + 1)]).await, (0, 0));
    }

    #[test]
    fn file_uris_become_local_paths() {
        for (uri, expected) in [
            ("file:///srv/code", Some("/srv/code")),
            ("file://localhost/srv/code", Some("/srv/code")),
            ("file:///srv/My%20Code/%D0%BA", Some("/srv/My Code/к")),
            ("file://server/share", None),
            ("https://example.com/x", None),
            ("file:///bad%2", None),
            ("file:///bad%zz", None),
            ("file:///bad%+1", None),
            ("file:///not-utf8%FF", None),
        ] {
            assert_eq!(file_uri_path(uri), expected.map(PathBuf::from), "{uri}");
        }
    }
}
