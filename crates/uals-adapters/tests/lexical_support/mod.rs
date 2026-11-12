//! Helpers shared by the `lexical*` integration tests. Everything goes through the public API.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;
use uals_adapters::lexical::LexicalBackend;
use uals_core::backend::{BackendError, SearchBackend};
use uals_core::config::LexicalConfig;
use uals_core::types::{BackendAnswer, QueryKind, Repo, SearchRequest};

pub const DEADLINE: Duration = Duration::from_secs(30);

pub fn temp() -> TempDir {
    tempfile::tempdir().expect("temp dir")
}

pub fn write(root: &Path, relative: &str, content: &str) {
    write_bytes(root, relative, content.as_bytes());
}

pub fn write_bytes(root: &Path, relative: &str, content: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    fs::write(path, content).expect("write file");
}

/// Runs `git <args>` in `dir` with a config that does not depend on the user's.
pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git is installed");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

pub fn git_init(dir: &Path) {
    fs::create_dir_all(dir).expect("create repo dir");
    git(dir, &["init", "-q"]);
}

/// A repository handle with a canonical root (macOS temp dirs sit behind a symlink).
pub fn repo(root: &Path) -> Repo {
    let root = root.canonicalize().expect("canonical root");
    let name = root.file_name().map_or_else(|| "repo".to_owned(), |name| name.to_string_lossy().into_owned());
    Repo { name, root }
}

pub fn request(kind: QueryKind, query: &str) -> SearchRequest {
    SearchRequest { query: query.to_owned(), kind, fetch_limit: 1000, paths: Vec::new() }
}

pub fn backend() -> LexicalBackend {
    LexicalBackend::new(LexicalConfig::default(), DEADLINE)
}

pub async fn find(repo: &Repo, kind: QueryKind, query: &str) -> BackendAnswer {
    backend().search(repo, &request(kind, query)).await.expect("search succeeds")
}

pub async fn find_with(config: LexicalConfig, repo: &Repo, kind: QueryKind, query: &str) -> BackendAnswer {
    LexicalBackend::new(config, DEADLINE).search(repo, &request(kind, query)).await.expect("search succeeds")
}

pub async fn try_find(repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
    backend().search(repo, request).await
}

/// Hit paths relative to the repository root, `/`-separated, sorted and without duplicates.
pub fn files(repo: &Repo, answer: &BackendAnswer) -> Vec<String> {
    let mut files: Vec<String> = answer.hits.iter().map(|hit| relative(repo, &hit.path)).collect();
    files.sort();
    files.dedup();
    files
}

pub fn relative(repo: &Repo, path: &Path) -> String {
    let relative = path.strip_prefix(&repo.root).unwrap_or_else(|_| panic!("{} is outside the repo", path.display()));
    relative.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// A copy of the public fixture workspace with `git init` in each project.
pub fn fixture_copy(into: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    let target = into.join("workspace");
    copy_dir(&source, &target);
    for entry in fs::read_dir(&target).expect("read workspace") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            git_init(&path);
        }
    }
    target
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create dir");
    for entry in fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy file");
        }
    }
}
