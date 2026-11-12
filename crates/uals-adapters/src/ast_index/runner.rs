//! Process execution behind a small seam, so tests can feed recorded outputs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;
use uals_core::backend::BackendError;

/// What an `ast-index` run printed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the process was killed by a signal.
    pub exit_code: Option<i32>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Runs `ast-index` for one repository. The real implementation spawns the binary; tests replace
/// it with recorded outputs.
#[async_trait]
pub trait AstIndexRunner: Send + Sync {
    /// Runs `ast-index <args...>` with `root` as the working directory.
    async fn run(&self, root: &Path, args: &[String]) -> Result<CommandOutput, BackendError>;

    /// Whether an index database already exists for `root`. Must not touch the cache: any
    /// `ast-index` command creates a stub cache folder for a project it has not seen.
    fn index_exists(&self, root: &Path) -> bool;
}

/// Spawns the real binary.
pub struct ProcessRunner {
    binary: String,
    /// Extra variables for the child (and for locating its cache); the rest of the environment
    /// is passed through unchanged. Tests use it to point `AST_INDEX_CACHE_DIR` at a temp folder.
    env: Vec<(String, OsString)>,
}

impl ProcessRunner {
    pub fn new(binary: impl Into<String>) -> Self {
        Self { binary: binary.into(), env: Vec::new() }
    }

    /// Sets one environment variable for every child process.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    fn var(&self, name: &str) -> Option<OsString> {
        let own = self.env.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        own.or_else(|| std::env::var_os(name)).filter(|value| !value.is_empty())
    }
}

#[async_trait]
impl AstIndexRunner for ProcessRunner {
    async fn run(&self, root: &Path, args: &[String]) -> Result<CommandOutput, BackendError> {
        let program =
            which::which(&self.binary).map_err(|_| BackendError::Unavailable("ast-index is not installed".into()))?;
        let output = Command::new(program)
            .args(args)
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| BackendError::Failed(format!("cannot run ast-index: {error}")))?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code(),
        })
    }

    fn index_exists(&self, root: &Path) -> bool {
        self.index_db_path(root).is_some_and(|path| path.is_file())
    }
}

impl ProcessRunner {
    /// Where ast-index keeps the database of `root`: `<cache>/<djb2 hash of the root>/index.db`,
    /// or the file named by `AST_INDEX_DB_PATH`. `None` when no cache folder can be determined.
    fn index_db_path(&self, root: &Path) -> Option<PathBuf> {
        if let Some(path) = self.var("AST_INDEX_DB_PATH").or_else(|| self.var("KOTLIN_INDEX_DB_PATH")) {
            return Some(PathBuf::from(path));
        }
        let base = match self.var("AST_INDEX_CACHE_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => self.default_cache_base()?,
        };
        Some(base.join(djb2(&root.to_string_lossy())).join("index.db"))
    }

    fn default_cache_base(&self) -> Option<PathBuf> {
        let home = || self.var("HOME").map(PathBuf::from);
        if cfg!(target_os = "macos") {
            return Some(home()?.join("Library").join("Caches").join("ast-index"));
        }
        let base = match self.var("XDG_CACHE_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => home()?.join(".cache"),
        };
        Some(base.join("ast-index"))
    }
}

/// The cache folder name ast-index derives from the project root (djb2, lower-case hex).
fn djb2(text: &str) -> String {
    let mut hash: u64 = 5381;
    for byte in text.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(byte));
    }
    format!("{hash:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn djb2_matches_the_known_empty_hash() {
        assert_eq!(djb2(""), "1505");
        assert_eq!(djb2("a"), "2b606");
    }
}
