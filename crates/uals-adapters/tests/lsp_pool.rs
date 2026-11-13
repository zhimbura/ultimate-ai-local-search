//! The language-server pool against `fixtures/fake_lsp.py`, a server that logs what it gets.
//! Needs `python3` on PATH.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use serde_json::Value;
use uals_adapters::lsp::LspPool;
use uals_core::backend::{BackendError, ReferenceResolver};
use uals_core::config::{LspConfig, LspServerConfig};
use uals_core::types::{Definition, Reference, Repo, SymbolKind};

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_lsp.py");

/// A folder with a Gradle project (Kotlin) at its root and a Go module in `tools/`.
struct MixedRepo {
    _tmp: tempfile::TempDir,
    repo: Repo,
    logs: PathBuf,
}

impl MixedRepo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("settings.gradle.kts", "");
        write("app/Store.kt", "class Store {\n    fun load() = 1\n}\n");
        write("app/Use.kt", "fun main() {\n\n    Store().load()\n}\n");
        write("tools/go.mod", "module tools\n");
        write("tools/main.go", "package main\n\nfunc Load() {}\n");
        write("tools/use.go", "package main\n\nfunc x() { Load() }\n");
        write("native/load.c", "int load(void) { return 1; }\n");
        let logs = root.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        Self { repo: Repo { name: "mixed".into(), root }, logs, _tmp: tmp }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.repo.root.join(relative)
    }

    /// A fake server for `name` that answers with `reference` and logs to `logs/<name>.jsonl`.
    fn server(&self, name: &str, reference: &str, extra: &[(&str, &str)]) -> (String, LspServerConfig) {
        let mut env = BTreeMap::from([
            ("FAKE_LOG".to_owned(), self.log_path(name).display().to_string()),
            ("FAKE_REF".to_owned(), reference.to_owned()),
        ]);
        env.extend(extra.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        let command = vec!["python3".into(), FAKE.into(), "--system-path={state}".into()];
        (name.to_owned(), LspServerConfig { command, env, ..LspServerConfig::default() })
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.logs.join(format!("{name}.jsonl"))
    }

    /// Every message the fake server `name` received.
    fn log(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(self.log_path(name))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn methods(&self, name: &str) -> Vec<String> {
        self.log(name).iter().filter_map(|m| m["method"].as_str().map(str::to_owned)).collect()
    }

    fn definition(&self, path: &str, line: u32, name: &str) -> Definition {
        Definition {
            repo: self.repo.name.clone(),
            path: self.path(path),
            line,
            name: name.into(),
            kind: SymbolKind::Function,
            container: None,
        }
    }
}

fn pool(servers: Vec<(String, LspServerConfig)>, ready_wait_ms: u64, state_root: &Path) -> LspPool {
    let config =
        LspConfig { ready_wait_ms, request_ms: 5_000, servers: servers.into_iter().collect(), ..LspConfig::default() };
    LspPool::new(config, state_root.to_path_buf())
}

fn unavailable(result: Result<impl std::fmt::Debug, BackendError>) -> String {
    match result {
        Err(BackendError::Unavailable(reason)) => reason,
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn a_go_module_and_a_gradle_project_in_one_folder_get_a_server_each() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let pool = pool(
        vec![fx.server("kotlin-lsp", "app/Use.kt", &[("FAKE_PROGRESS", "full")]), fx.server("gopls", "use.go", &[])],
        10_000,
        state.path(),
    );

    let (store_load, go_load) = (fx.definition("app/Store.kt", 2, "load"), fx.definition("tools/main.go", 3, "Load"));
    let (kotlin, go) = tokio::join!(pool.references(&fx.repo, &store_load), pool.references(&fx.repo, &go_load));

    let kotlin = kotlin.unwrap();
    assert_eq!(kotlin.server, "kotlin-lsp");
    assert_eq!(kotlin.root, fx.repo.root);
    assert_eq!(kotlin.extensions, ["kt", "kts"]);
    assert_eq!(kotlin.references, [Reference { path: fx.path("app/Use.kt"), line: 3 }]);
    let go = go.unwrap();
    assert_eq!(go.server, "gopls");
    assert_eq!(go.root, fx.path("tools"));
    assert_eq!(go.references, [Reference { path: fx.path("tools/use.go"), line: 3 }]);

    // Each server runs in its own project folder and got the position of the name.
    let kotlin_log = fx.log("kotlin-lsp");
    let go_log = fx.log("gopls");
    assert_eq!(kotlin_log[0]["cwd"], fx.repo.root.display().to_string());
    assert_eq!(go_log[0]["cwd"], fx.path("tools").display().to_string());
    let position = |log: &[Value]| {
        log.iter().find(|m| m["method"] == "textDocument/references").map(|m| m["params"]["position"].clone())
    };
    assert_eq!(position(&kotlin_log), Some(serde_json::json!({ "line": 1, "character": 8 })));
    assert_eq!(position(&go_log), Some(serde_json::json!({ "line": 2, "character": 5 })));
    // The configuration request got one null per item.
    let reply = go_log.iter().find(|m| m["method"].is_null()).expect("a reply to the configuration request");
    assert_eq!(reply["result"], serde_json::json!([null, null]));
    // `{state}` became a folder of this server under the state root.
    let arg = kotlin_log[0]["args"][0].as_str().unwrap().to_owned();
    assert!(arg.starts_with(&format!("--system-path={}", state.path().join("kotlin-lsp").display())), "{arg}");

    let servers = pool.servers();
    assert_eq!(
        servers,
        [
            format!("gopls {}: ready", fx.path("tools").display()),
            format!("kotlin-lsp {}: ready", fx.repo.root.display())
        ]
    );
    pool.shutdown().await;
    // `shutdown` is answered after everything before it, so the log is complete up to it.
    let expected = [
        "initialize",
        "initialized",
        "textDocument/didOpen",
        "textDocument/references",
        "textDocument/didClose",
        "shutdown",
    ];
    assert_eq!(fx.methods("gopls")[..expected.len()], expected);
}

#[tokio::test]
async fn a_server_still_loading_answers_unavailable_and_keeps_loading() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let pool = pool(vec![fx.server("kotlin-lsp", "app/Use.kt", &[("FAKE_PROGRESS", "begin")])], 300, state.path());

    let reason = unavailable(pool.references(&fx.repo, &fx.definition("app/Store.kt", 2, "load")).await);

    assert!(reason.starts_with("kotlin-lsp is still loading"), "{reason}");
    assert!(!fx.methods("kotlin-lsp").contains(&"textDocument/references".to_owned()));
    // The server goes on running: a second call does not start another one.
    let _ = pool.references(&fx.repo, &fx.definition("app/Store.kt", 2, "load")).await;
    assert_eq!(fx.methods("kotlin-lsp").iter().filter(|m| *m == "initialize").count(), 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn a_call_dropped_while_the_server_answers_closes_its_document() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let pool = pool(vec![fx.server("gopls", "use.go", &[("FAKE_DELAY", "1")])], 10_000, state.path());
    let go_load = fx.definition("tools/main.go", 3, "Load");
    let asked = || fx.methods("gopls").contains(&"textDocument/references".to_owned());

    // Boxed so that `drop` ends the call itself, not a pin of it.
    let mut call = Box::pin(pool.references(&fx.repo, &go_load));
    let waiting = async {
        while !asked() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        result = &mut call => panic!("the call ended before it was dropped: {result:?}"),
        () = waiting => {}
    }
    drop(call);
    let again = pool.references(&fx.repo, &go_load).await.unwrap();

    assert_eq!(again.references, [Reference { path: fx.path("tools/use.go"), line: 3 }]);
    pool.shutdown().await;
    // The dropped call closed its document, so the next one opened it again.
    let expected = [
        "initialize",
        "initialized",
        "textDocument/didOpen",
        "textDocument/references",
        "textDocument/didClose",
        "textDocument/didOpen",
        "textDocument/references",
        "textDocument/didClose",
        "shutdown",
    ];
    assert_eq!(fx.methods("gopls")[..expected.len()], expected);
}

#[tokio::test]
async fn a_project_that_does_not_load_is_not_retried_at_once() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let extra = [("FAKE_PROGRESS", "full"), ("FAKE_FAIL", "Unable to import project: no JDK")];
    let pool = pool(vec![fx.server("kotlin-lsp", "app/Use.kt", &extra)], 10_000, state.path());

    let first = unavailable(pool.references(&fx.repo, &fx.definition("app/Store.kt", 2, "load")).await);
    let second = unavailable(pool.references(&fx.repo, &fx.definition("app/Store.kt", 2, "load")).await);

    assert_eq!(first, "kotlin-lsp could not load the project: Unable to import project: no JDK");
    assert_eq!(second, first);
    assert_eq!(fx.methods("kotlin-lsp").iter().filter(|m| *m == "initialize").count(), 1);
}

#[tokio::test]
async fn files_without_a_server_and_turned_off_servers_are_unavailable() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let off = LspServerConfig { enabled: false, ..LspServerConfig::default() };
    let pool = pool(vec![("gopls".into(), off)], 1_000, state.path());

    let markdown = unavailable(pool.references(&fx.repo, &fx.definition("README.md", 1, "x")).await);
    let go = unavailable(pool.references(&fx.repo, &fx.definition("tools/main.go", 3, "Load")).await);

    assert!(markdown.starts_with("no language server handles "), "{markdown}");
    assert_eq!(go, "gopls is turned off in the config");
}

#[tokio::test]
async fn a_server_that_is_not_installed_is_named() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let missing = LspServerConfig { command: vec!["uals-no-such-server".into()], ..LspServerConfig::default() };
    let pool = pool(vec![("gopls".into(), missing)], 1_000, state.path());

    let reason = unavailable(pool.references(&fx.repo, &fx.definition("tools/main.go", 3, "Load")).await);

    assert_eq!(reason, "gopls is not installed (looked for uals-no-such-server)");
}

#[tokio::test]
async fn a_stale_line_is_reported_instead_of_asking_the_server() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let pool = pool(vec![fx.server("gopls", "use.go", &[])], 10_000, state.path());

    let reason = unavailable(pool.references(&fx.repo, &fx.definition("tools/main.go", 1, "Load")).await);

    assert!(reason.starts_with("'Load' is not on line 1 of "), "{reason}");
    pool.shutdown().await;
}

#[tokio::test]
async fn an_empty_answer_counts_only_from_servers_that_see_the_whole_project() {
    let fx = MixedRepo::new();
    let state = tempfile::tempdir().unwrap();
    let pool = pool(vec![fx.server("gopls", "", &[]), fx.server("clangd", "", &[])], 10_000, state.path());

    let go = pool.references(&fx.repo, &fx.definition("tools/main.go", 3, "Load")).await.unwrap();
    let c = unavailable(pool.references(&fx.repo, &fx.definition("native/load.c", 1, "load")).await);

    assert!(go.references.is_empty());
    assert_eq!(c, format!("clangd found no usages; its index of {} may not be built", fx.repo.root.display()));
    pool.shutdown().await;
}
