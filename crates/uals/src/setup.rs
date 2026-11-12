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

fn semantic_backend(config: &Config) -> Option<Arc<dyn SearchBackend>> {
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

/// A server that holds a lock while it runs. Fields drop in order: the server stops first, then
/// the lock is released.
struct Locked {
    server: McpChild,
    _lock: File,
}

#[async_trait]
impl McpToolCaller for Locked {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolOutput, BackendError> {
        self.server.call_tool(name, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
