//! `uals hook post-edit`: a Claude Code `PostToolUse` hook for file edits. It reads the hook input
//! on stdin, finds the repository that holds the edited file, and runs an incremental
//! `ast-index update` there when that repository already has an index. Creating an index is left
//! to `uals refresh`. Input without a file path is ignored, and so is a file that no repository
//! holds (a note or a plan outside the workspace): it is not an error.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tracing::debug;
use uals_adapters::ast_index::AstIndexBackend;
use uals_core::backend::SearchBackend;
use uals_core::config::Config;
use uals_core::types::Repo;
use uals_core::workspace::{self, WorkspaceError};

/// How long the update may take. After this the hook stops `ast-index update` itself (the process
/// gets SIGKILL when the call is dropped) and exits 1 with a timeout error. Claude Code's default
/// hook timeout is 60 s, so the hook ends first.
const UPDATE_DEADLINE: Duration = Duration::from_secs(50);

/// The part of the hook input this hook reads.
#[derive(Debug, Deserialize)]
struct HookInput {
    /// The session's folder; relative file paths are resolved against it.
    cwd: Option<PathBuf>,
    #[serde(default)]
    tool_input: ToolInput,
}

/// The file an edit tool changed: `file_path` (Edit, MultiEdit, Write) or `notebook_path`
/// (NotebookEdit).
#[derive(Debug, Default, Deserialize)]
struct ToolInput {
    file_path: Option<PathBuf>,
    notebook_path: Option<PathBuf>,
}

/// Runs the hook on `input`, the hook's stdin.
pub async fn post_edit(config: &Config, input: impl Read) -> Result<()> {
    let input: HookInput = serde_json::from_reader(input).context("cannot read the hook input")?;
    let Some(file) = edited_file(input) else {
        debug!("the hook input names no file");
        return Ok(());
    };
    let Some(repo) = repo_of(&file, config)? else {
        debug!(file = %file.display(), "no repository holds the file");
        return Ok(());
    };
    let ast_index = AstIndexBackend::new(config.ast_index.clone());
    if !ast_index.index_state(&repo).await.indexed {
        debug!(repo = %repo.name, "no ast-index index; not creating one");
        return Ok(());
    }
    tokio::time::timeout(UPDATE_DEADLINE, ast_index.refresh(&repo, false))
        .await
        .map_err(|_| anyhow!("ast-index update of {} took longer than {UPDATE_DEADLINE:?}", repo.name))?
        .with_context(|| format!("ast-index update of {} failed", repo.name))
}

/// The absolute path of the edited file, or `None` when the input names none.
fn edited_file(input: HookInput) -> Option<PathBuf> {
    let file = input.tool_input.file_path.or(input.tool_input.notebook_path)?;
    match input.cwd {
        Some(cwd) if file.is_relative() => Some(cwd.join(file)),
        _ => Some(file),
    }
}

/// The repository that holds `file`: the deepest repository of the workspace discovered from the
/// file's folder, or `None` when there is no such repository.
fn repo_of(file: &Path, config: &Config) -> Result<Option<Repo>> {
    let folder = file.parent().with_context(|| format!("{} has no folder", file.display()))?;
    let folder = folder.canonicalize().with_context(|| format!("cannot read {}", folder.display()))?;
    let found = match workspace::discover(&folder, config) {
        Ok(found) => found,
        Err(WorkspaceError::NoRepos(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(found
        .repos
        .into_iter()
        .filter(|repo| folder.starts_with(&repo.root))
        .max_by_key(|repo| repo.root.components().count()))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use uals_core::config::{RepoConfig, WorkspaceConfig};

    use super::*;

    fn input(json: &str) -> HookInput {
        serde_json::from_str(json).expect("hook input")
    }

    #[test]
    fn the_edited_file_comes_from_the_tool_input() {
        let cases = [
            (r#"{"cwd": "/ws", "tool_input": {"file_path": "/ws/a/B.kt"}}"#, Some("/ws/a/B.kt")),
            (r#"{"cwd": "/ws", "tool_input": {"file_path": "a/B.kt"}}"#, Some("/ws/a/B.kt")),
            (r#"{"tool_input": {"notebook_path": "/ws/n.ipynb"}}"#, Some("/ws/n.ipynb")),
            (r#"{"cwd": "/ws", "tool_input": {"command": "ls"}}"#, None),
            (r#"{"cwd": "/ws"}"#, None),
        ];
        for (json, expected) in cases {
            assert_eq!(edited_file(input(json)), expected.map(PathBuf::from), "{json}");
        }
    }

    #[test]
    fn the_repository_is_the_nearest_one_that_holds_the_file() {
        // Only a configured workspace can list a repository inside another one.
        let root = tempfile::tempdir().expect("temp dir");
        let outer = root.path().canonicalize().expect("canonical").join("app");
        let inner = outer.join("libs/inner");
        std::fs::create_dir_all(inner.join("src")).expect("folders");
        let mut config = Config::default();
        let repos = vec![
            RepoConfig { name: "app".into(), path: outer.clone() },
            RepoConfig { name: "lib".into(), path: inner.clone() },
        ];
        config.workspaces.insert("nested".into(), WorkspaceConfig { root: outer.clone(), exclude: Vec::new(), repos });

        let repo = repo_of(&inner.join("src/Main.kt"), &config).expect("no error").expect("a repository");

        assert_eq!(repo.root, inner);
    }

    #[test]
    fn a_file_in_a_workspace_but_in_no_repository_has_no_repository() {
        let root = tempfile::tempdir().expect("temp dir");
        let outer = root.path().canonicalize().expect("canonical").join("ws");
        std::fs::create_dir_all(outer.join("lib")).expect("folders");
        std::fs::create_dir_all(outer.join("docs")).expect("folders");
        let mut config = Config::default();
        let repos = vec![RepoConfig { name: "lib".into(), path: outer.join("lib") }];
        config.workspaces.insert("ws".into(), WorkspaceConfig { root: outer.clone(), exclude: Vec::new(), repos });

        let repo = repo_of(&outer.join("docs/plan.md"), &config).expect("no error");

        assert_eq!(repo, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_read_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temp dir");
        let locked = root.path().join("locked");
        std::fs::create_dir(&locked).expect("folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("lock");
        let readable = std::fs::read_dir(&locked).is_ok();

        let result = repo_of(&locked.join("x.txt"), &Config::default());

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("unlock");
        if !readable {
            assert!(result.is_err(), "{result:?}");
        }
    }

    #[test]
    fn a_file_outside_any_repository_has_no_repository() {
        let root = tempfile::tempdir().expect("temp dir");

        let repo = repo_of(&root.path().join("loose.txt"), &Config::default()).expect("no error");

        assert_eq!(repo, None);
    }

    #[test]
    fn a_file_in_a_missing_folder_is_an_error() {
        let root = tempfile::tempdir().expect("temp dir");

        let error = repo_of(&root.path().join("gone/loose.txt"), &Config::default()).expect_err("missing folder");

        assert!(error.to_string().contains("cannot read"), "{error:#}");
    }
}
