//! Builds the backends the config enables.

use std::fs::{File, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use uals_adapters::ast_index::AstIndexBackend;
use uals_adapters::claude_context::ClaudeContextBackend;
use uals_adapters::lexical::LexicalBackend;
use uals_adapters::mcp_child::{ChildSpec, McpChild};
use uals_adapters::semble::SembleBackend;
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend, SpanProvider, ToolOutput, UsageSource};
use uals_core::config::{ClaudeContextConfig, Config, SemanticChoice, SembleConfig};

use crate::lazy_caller::LazyCaller;
use crate::orchestrator::{Backends, Orchestrator};

/// Where the uals process that runs claude-context keeps its lock, relative to the home folder.
const CLAUDE_CONTEXT_LOCK: &str = ".local/state/uals/claude-context.lock";

/// How long a locked server gets to end after SIGTERM before it is killed.
const LOCKED_SERVER_STOP_BOUND: Duration = Duration::from_secs(1);

/// An orchestrator with every backend the config enables.
pub fn orchestrator(config: Config) -> Orchestrator {
    Orchestrator::new(backends(&config), config)
}

/// ast-index (search, usages, spans), lexical search (search, and usages where ast-index cannot
/// answer) and the semantic backend `semantic.default` names. Semantic servers start on their
/// first call.
pub fn backends(config: &Config) -> Backends {
    let ast_index = Arc::new(AstIndexBackend::new(config.ast_index.clone()));
    let lexical =
        Arc::new(LexicalBackend::new(config.lexical.clone(), Duration::from_millis(config.deadlines.lexical_ms)));

    let mut search: Vec<Arc<dyn SearchBackend>> = Vec::with_capacity(3);
    search.push(ast_index.clone());
    search.push(lexical.clone());
    if let Some(semantic) = semantic_backend(config) {
        search.push(semantic);
    }
    let usages: Vec<Arc<dyn UsageSource>> = vec![ast_index.clone(), lexical];
    let spans: Arc<dyn SpanProvider> = ast_index;
    Backends { search, usages, spans: Some(spans) }
}

/// The semantic backend `semantic.default` names, behind a `LazyCaller`; `None` for `none`.
pub fn semantic_backend(config: &Config) -> Option<Arc<dyn SearchBackend>> {
    match config.semantic.default {
        SemanticChoice::None => None,
        SemanticChoice::Semble => {
            let semble = config.semantic.semble.clone();
            let for_start = semble.clone();
            let caller = LazyCaller::new("semble", Arc::new(move || start_semble(for_start.clone()).boxed()));
            Some(Arc::new(SembleBackend::new(Arc::new(caller), semble)))
        }
        SemanticChoice::ClaudeContext => {
            let claude_context = config.semantic.claude_context.clone();
            let for_start = claude_context.clone();
            let caller =
                LazyCaller::new("claude-context", Arc::new(move || start_claude_context(for_start.clone()).boxed()));
            Some(Arc::new(ClaudeContextBackend::new(Arc::new(caller), claude_context)))
        }
    }
}

/// Starts the semble server as a child process.
async fn start_semble(config: SembleConfig) -> Result<Arc<dyn McpToolCaller>, BackendError> {
    let (program, args) = config
        .command
        .split_first()
        .ok_or_else(|| BackendError::Unavailable("semantic.semble.command is empty".into()))?;
    let spec = ChildSpec {
        name: "semble".into(),
        program: PathBuf::from(program),
        args: args.to_vec(),
        env: config.env,
        pass_env: config.pass_env,
        cwd: None,
    };
    Ok(Arc::new(McpChild::spawn(spec).await?))
}

/// Starts the claude-context server under the single-instance lock, so no other uals process
/// runs it at the same time.
async fn start_claude_context(config: ClaudeContextConfig) -> Result<Arc<dyn McpToolCaller>, BackendError> {
    let entry = config.entry_path().ok_or_else(|| {
        BackendError::Unavailable("cannot find the home folder, so the claude-context entry point is unknown".into())
    })?;
    if !entry.is_file() {
        return Err(BackendError::Unavailable(format!(
            "claude-context is not installed: {} does not exist",
            entry.display()
        )));
    }
    let home = dirs::home_dir().ok_or_else(|| BackendError::Unavailable("cannot find the home folder".into()))?;
    let lock = lock_single_instance(&home.join(CLAUDE_CONTEXT_LOCK), "claude-context")?;
    let spec = ChildSpec {
        name: "claude-context".into(),
        program: PathBuf::from(&config.node),
        args: vec![entry.to_string_lossy().into_owned()],
        env: config.env,
        pass_env: config.pass_env,
        cwd: None,
    };
    let server = McpChild::spawn(spec).await?;
    Ok(Arc::new(Locked { server, _lock: lock }))
}

/// Takes the lock that lets only one uals process run `what`, and writes this process id into
/// the lock file. claude-context needs it: it rewrites one snapshot file in `~/.context` and syncs
/// every indexed codebase in the background, so two servers on one machine race each other.
/// Another holder → `Unavailable` with its process id.
fn lock_single_instance(path: &Path, what: &str) -> Result<File, BackendError> {
    let failed =
        |error: std::io::Error| BackendError::Failed(format!("cannot use the lock {}: {error}", path.display()));
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(failed)?;
    }
    let mut file = File::options().read(true).write(true).create(true).truncate(false).open(path).map_err(failed)?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0).map_err(failed)?;
            file.write_all(std::process::id().to_string().as_bytes()).map_err(failed)?;
            Ok(file)
        }
        Err(TryLockError::WouldBlock) => {
            let mut holder = String::new();
            // The process id is only for the message; an unreadable file still means "taken".
            let _ = file.read_to_string(&mut holder);
            Err(BackendError::Unavailable(format!(
                "{what} is already run by another uals process (pid {}); only one may run it",
                holder.trim()
            )))
        }
        Err(TryLockError::Error(error)) => Err(failed(error)),
    }
}

/// A server that holds a lock while it runs. Dropping it stops the server's process group and
/// waits for it to end, then the lock is released, so no other process can start a second server
/// while this one still runs.
struct Locked {
    server: McpChild,
    _lock: File,
}

impl Drop for Locked {
    fn drop(&mut self) {
        self.server.stop_and_wait(LOCKED_SERVER_STOP_BOUND);
    }
}

#[async_trait]
impl McpToolCaller for Locked {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolOutput, BackendError> {
        self.server.call_tool(name, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::Instant;

    use super::*;

    /// An MCP server that ignores SIGTERM and answers only `initialize`.
    const STUBBORN_SERVER: &str = r#"#!/bin/sh
trap '' TERM
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}\n' "$id" ;;
  esac
done
"#;

    /// Whether the group has a process that is not a zombie: Linux keeps an unreaped zombie in
    /// its group, and this test process, the parent, does not reap while the drop blocks.
    fn group_exists(pgid: i32) -> bool {
        let out = Command::new("ps").args(["-A", "-o", "pgid=,stat="]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next() == Some(&pgid.to_string()) && fields.next().is_some_and(|stat| !stat.starts_with('Z'))
        })
    }

    #[test]
    fn only_one_holder_gets_the_lock() {
        let folder = tempfile::tempdir().expect("temp dir");
        let path = folder.path().join("state/claude-context.lock");

        let first = lock_single_instance(&path, "claude-context").expect("the first holder gets the lock");
        let second = lock_single_instance(&path, "claude-context");
        let pid = std::process::id().to_string();
        assert!(
            matches!(&second, Err(BackendError::Unavailable(reason)) if reason.contains(&pid)),
            "unexpected {second:?}"
        );

        drop(first);
        assert!(lock_single_instance(&path, "claude-context").is_ok(), "the lock is free after the holder drops it");
    }

    // A current-thread runtime: tokio cannot kill the child behind the test's back while it blocks.
    #[tokio::test]
    async fn the_lock_is_released_only_after_a_stubborn_server_is_dead() {
        let folder = tempfile::tempdir().expect("temp dir");
        let script = folder.path().join("stubborn-server");
        std::fs::write(&script, STUBBORN_SERVER).expect("write the server");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("make it executable");
        let lock_path = folder.path().join("state/server.lock");
        let lock = lock_single_instance(&lock_path, "stubborn").expect("the lock is free");
        let spec = ChildSpec {
            name: "stubborn".into(),
            program: script,
            args: vec![],
            env: BTreeMap::new(),
            pass_env: vec![],
            cwd: None,
        };
        let server = McpChild::spawn(spec).await.expect("the server starts");
        let pgid = server.process_group().expect("a running server has a group");
        assert!(group_exists(pgid));

        let started = Instant::now();
        drop(Locked { server, _lock: lock });

        assert!(!group_exists(pgid), "the server must be dead when the drop returns");
        assert!(started.elapsed() < Duration::from_secs(3), "the wait is bounded");
        assert!(lock_single_instance(&lock_path, "stubborn").is_ok(), "the lock is free once the server is dead");
    }
}
