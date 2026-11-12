//! `SembleBackend` over a real `McpChild` running the real `semble` on a temp copy of the public
//! fixture workspace. Ignored by default (`cargo test -- --ignored` runs them); a missing `semble`
//! on PATH fails them. Semble's cache is redirected to the temp folder with `SEMBLE_CACHE_LOCATION`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use uals_adapters::mcp_child::{ChildSpec, McpChild};
use uals_adapters::semble::SembleBackend;
use uals_core::backend::{BackendError, SearchBackend};
use uals_core::config::{SembleConfig, SembleContent};
use uals_core::types::{BackendId, LineRange, QueryKind, Repo, SearchRequest};

struct Live {
    _dir: TempDir,
    workspace: PathBuf,
    spec: ChildSpec,
}

fn live() -> Live {
    let semble = which::which("semble").expect("live test needs `semble` on PATH (install Semble 0.7.x)");
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace"), &workspace);
    for project in ["sample-android", "sample-tools", "sample-web"] {
        let status = Command::new("git").args(["init", "-q"]).current_dir(workspace.join(project)).status().unwrap();
        assert!(status.success());
    }
    let cache = dir.path().join("semble-cache");
    std::fs::create_dir_all(&cache).unwrap();
    let spec = ChildSpec {
        name: "semble".into(),
        program: semble,
        args: vec![],
        env: BTreeMap::from([("SEMBLE_CACHE_LOCATION".to_owned(), cache.display().to_string())]),
        pass_env: ["HF_HOME", "HF_ENDPOINT", "HF_HUB_OFFLINE", "XDG_CACHE_HOME"].map(String::from).to_vec(),
        cwd: None,
    };
    Live { _dir: dir, workspace, spec }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Live {
    fn repo(&self, name: &str) -> Repo {
        // Canonical like the real workspace scan (the temp folder may sit behind a symlink).
        Repo { name: name.to_owned(), root: self.workspace.join(name).canonicalize().unwrap() }
    }

    async fn backend(&self, content: Option<SembleContent>) -> SembleBackend {
        let child = McpChild::spawn(self.spec.clone()).await.unwrap();
        SembleBackend::new(Arc::new(child), SembleConfig { content, ..SembleConfig::default() })
    }
}

fn request(query: &str, kind: QueryKind, fetch_limit: usize) -> SearchRequest {
    SearchRequest { query: query.to_owned(), kind, fetch_limit, paths: vec![] }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs `semble` on PATH"]
async fn natural_and_identifier_queries_find_the_right_files() {
    let live = live();
    let backend = live.backend(None).await;
    let repo = live.repo("sample-android");

    let started = Instant::now();
    let answer =
        backend.search(&repo, &request("retry with exponential backoff", QueryKind::Natural, 5)).await.unwrap();
    eprintln!("MEASURED SembleBackend cold search {:?}", started.elapsed());
    let first = &answer.hits[0];
    assert_eq!(first.backend, BackendId::Semble);
    assert_eq!(first.repo, "sample-android");
    assert_eq!(first.path, repo.root.join("core/src/main/kotlin/sample/core/RetryPolicy.kt"));
    assert!(first.path.is_file(), "hit paths are absolute and exist");
    assert_eq!(first.lines, LineRange::new(1, 18));
    assert!(answer.hits.iter().enumerate().all(|(i, h)| h.rank as usize == i));
    assert!(answer.hits.len() <= 5);

    let started = Instant::now();
    let answer = backend.search(&repo, &request("HostSelector", QueryKind::Identifier, 3)).await.unwrap();
    eprintln!("MEASURED SembleBackend warm search {:?}", started.elapsed());
    assert!(answer.hits[0].path.ends_with("HostSelector.kt"), "{:?}", answer.hits[0].path);
    assert!(answer.hits.len() <= 3);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs `semble` on PATH"]
async fn path_globs_and_content_selection_work() {
    let live = live();
    let repo = live.repo("sample-android");

    let backend = live.backend(None).await;
    let mut req = request("retry with exponential backoff", QueryKind::Natural, 5);
    req.paths = vec!["network/**".into()];
    let answer = backend.search(&repo, &req).await.unwrap();
    assert!(!answer.hits.is_empty());
    assert!(answer.hits.iter().all(|h| h.path.starts_with(repo.root.join("network"))), "{answer:?}");

    // Docs are only indexed with content = docs/all: the README of the workspace root is not in a
    // project, so use the whole workspace as one repository.
    let root = Repo { name: "workspace".into(), root: live.workspace.canonicalize().unwrap() };
    let docs = live.backend(Some(SembleContent::Docs)).await;
    let answer = docs.search(&root, &request("readme overview", QueryKind::Natural, 5)).await.unwrap();
    assert!(answer.hits.iter().any(|h| h.path.extension().is_some_and(|e| e == "md")), "{answer:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs `semble` on PATH"]
async fn a_missing_repository_is_unavailable_and_the_backend_recovers() {
    let live = live();
    let backend = live.backend(None).await;
    let missing = Repo { name: "ghost".into(), root: live.workspace.join("ghost") };
    let error = backend.search(&missing, &request("anything", QueryKind::Natural, 5)).await.unwrap_err();
    let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
    assert!(message.contains("Failed to index"), "{message}");
    assert!(!backend.index_state(&missing).await.indexed);

    let ok = backend.search(&live.repo("sample-tools"), &request("cache", QueryKind::Natural, 3)).await.unwrap();
    assert!(!ok.hits.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs `semble` on PATH"]
async fn warm_builds_indexes_in_the_background() {
    let live = live();
    let backend = live.backend(None).await;
    let repos = [live.repo("sample-android"), live.repo("sample-tools"), live.repo("sample-web")];
    for repo in &repos {
        assert!(!backend.index_state(repo).await.indexed);
    }

    let started = Instant::now();
    backend.warm(&repos).await.unwrap();
    let returned_after = started.elapsed();
    assert!(returned_after < Duration::from_millis(300), "warm must return at once, took {returned_after:?}");

    for repo in &repos {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !backend.index_state(repo).await.indexed {
            assert!(Instant::now() < deadline, "{} was not indexed in time", repo.name);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    eprintln!("MEASURED warm returned in {returned_after:?}, 3 repos indexed in {:?}", started.elapsed());

    let started = Instant::now();
    let answer =
        backend.search(&repos[0], &request("retry with exponential backoff", QueryKind::Natural, 3)).await.unwrap();
    eprintln!("MEASURED first real search after warm {:?}", started.elapsed());
    assert!(!answer.hits.is_empty());
    assert!(started.elapsed() < Duration::from_secs(2));
}
