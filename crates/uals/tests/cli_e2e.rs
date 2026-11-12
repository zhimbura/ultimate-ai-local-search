//! The `uals` commands on the fixture workspace with a fake ast-index: a `sh` script that
//! logs the folder and arguments of every run, answers the index-state query of an indexed
//! repository, and fails `update` with "disk full" when `FAKE_AST_INDEX_FAIL` is set.
#![cfg(unix)]

#[path = "../../uals-adapters/tests/lexical_support/mod.rs"]
mod lexical_support;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use lexical_support::fixture_copy;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;
use uals_core::types::{BackendId, SearchResponse, UsagesResponse};

const FAKE_AST_INDEX: &str = r#"#!/bin/sh
echo "$(pwd -P) $1" >> "$FAKE_AST_INDEX_LOG"
case "$1" in
  query) printf '%s' '{"columns":["updated","files","symbols"],"count":1,"rows":[{"files":7,"symbols":27,"updated":"1794478971901"}]}' ;;
  update|rebuild) if [ -n "$FAKE_AST_INDEX_FAIL" ]; then echo "disk full" >&2; exit 1; fi ;;
esac
"#;

/// Variables that point ast-index, and the adapter, at a database or cache folder.
const AST_INDEX_VARS: [&str; 3] = ["AST_INDEX_DB_PATH", "KOTLIN_INDEX_DB_PATH", "AST_INDEX_CACHE_DIR"];

struct Fixture {
    dir: TempDir,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let workspace = fixture_copy(dir.path()).canonicalize().unwrap();
        let script = dir.path().join("fake-ast-index");
        std::fs::write(&script, FAKE_AST_INDEX).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config =
            format!("[semantic]\ndefault = \"none\"\n\n[ast_index]\nbinary = {:?}\n", script.display().to_string());
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        std::fs::create_dir(dir.path().join("cache")).unwrap();
        Self { dir, workspace }
    }

    /// `uals <args>` with `stdin`. With `indexed`, every repository has an ast-index database.
    fn run(&self, args: &[&str], stdin: &str, indexed: bool, ast_index_fails: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_uals"));
        command.arg("--config").arg(self.dir.path().join("config.toml")).args(args);
        for name in AST_INDEX_VARS {
            command.env_remove(name);
        }
        if indexed {
            let database = self.dir.path().join("index.db");
            std::fs::write(&database, "").unwrap();
            command.env("AST_INDEX_DB_PATH", database);
        } else {
            command.env("AST_INDEX_CACHE_DIR", self.dir.path().join("cache"));
        }
        if ast_index_fails {
            command.env("FAKE_AST_INDEX_FAIL", "1");
        }
        let mut child = command
            .env("FAKE_AST_INDEX_LOG", self.log_file())
            .env_remove("UALS_WORKSPACE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("uals starts");
        child.stdin.take().expect("piped stdin").write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().expect("uals ends")
    }

    fn log_file(&self) -> PathBuf {
        self.dir.path().join("ast-index.log")
    }

    /// Every ast-index run: the folder and the command.
    fn ast_index_runs(&self) -> Vec<String> {
        std::fs::read_to_string(self.log_file()).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    /// The hook input of Claude Code after an edit of `relative` in the workspace.
    fn edit_of(&self, relative: &str) -> String {
        json!({
            "session_id": "e2e",
            "cwd": self.workspace,
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit",
            "tool_input": {"file_path": relative, "old_string": "a", "new_string": "b"}
        })
        .to_string()
    }
}

#[test]
fn an_edit_updates_the_index_of_the_repository_that_holds_the_file() {
    let fixture = Fixture::new();

    let output = fixture.run(&["hook", "post-edit"], &fixture.edit_of("sample-web/src/index.ts"), true, false);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let web = fixture.workspace.join("sample-web");
    assert_eq!(fixture.ast_index_runs(), [format!("{} query", web.display()), format!("{} update", web.display())]);
}

#[test]
fn an_edit_in_a_repository_without_an_index_runs_nothing() {
    let fixture = Fixture::new();

    let output = fixture.run(&["hook", "post-edit"], &fixture.edit_of("sample-web/src/index.ts"), false, false);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fixture.ast_index_runs(), Vec::<String>::new());
}

#[test]
fn a_failed_index_update_fails_the_hook_and_refresh() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.display().to_string();

    let failed_hook = fixture.run(&["hook", "post-edit"], &fixture.edit_of("sample-web/src/index.ts"), true, true);
    let failed_refresh = fixture.run(&["refresh", "--workspace", &workspace, "--repo", "sample-web"], "", true, true);
    let working_refresh = fixture.run(&["refresh", "--workspace", &workspace, "--repo", "sample-web"], "", true, false);

    assert_eq!(failed_hook.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&failed_hook.stderr).contains("disk full"),
        "{}",
        String::from_utf8_lossy(&failed_hook.stderr)
    );
    assert_eq!(failed_refresh.status.code(), Some(1));
    let printed = String::from_utf8_lossy(&failed_refresh.stdout);
    assert!(
        printed.starts_with("sample-web: ast-index failed (ast-index update failed (exit code 1): disk full)"),
        "{printed}"
    );
    assert_eq!(working_refresh.status.code(), Some(0), "{}", String::from_utf8_lossy(&working_refresh.stdout));
}

#[test]
fn an_edit_of_a_file_outside_every_repository_is_ignored() {
    let fixture = Fixture::new();
    let elsewhere = fixture.dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();

    let output =
        fixture.run(&["hook", "post-edit"], &fixture.edit_of(elsewhere.join("plan.md").to_str().unwrap()), true, false);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(fixture.ast_index_runs(), Vec::<String>::new());
}

/// `uals <args>` on a workspace without indexes (so only lexical search answers); its stdout.
fn printed(fixture: &Fixture, args: &[&str]) -> String {
    let output = fixture.run(args, "", false, false);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn search_prints_the_ranked_hits_of_the_workspace() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();

    let response: SearchResponse = serde_json::from_str(&printed(
        &fixture,
        &["search", "HostSelector", "--workspace", workspace, "--format", "json"],
    ))
    .unwrap();

    let top = &response.hits[0];
    assert_eq!(
        top.path,
        fixture.workspace.join("sample-android/network/src/main/kotlin/sample/network/HostSelector.kt")
    );
}

#[test]
fn usages_lists_every_use_of_a_symbol_in_the_named_repository() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();

    let response: UsagesResponse = serde_json::from_str(&printed(
        &fixture,
        &["usages", "slugify", "--workspace", workspace, "--repo", "sample-tools", "--format", "json"],
    ))
    .unwrap();

    assert_eq!(response.repos.len(), 1);
    assert_eq!(response.repos[0].source, BackendId::Lexical);
    let found: Vec<_> = response.repos[0].usages.iter().map(|usage| (usage.path.clone(), usage.line)).collect();
    let init = fixture.workspace.join("sample-tools/sample_tools/__init__.py");
    assert_eq!(found, [(init.clone(), 2), (init, 4)]);
}

#[test]
fn status_and_workspace_list_the_three_repositories() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let names = |value: &Value, key: &str| -> Vec<String> {
        value[key].as_array().unwrap().iter().map(|repo| repo["name"].as_str().unwrap().to_owned()).collect()
    };
    let expected = ["sample-android", "sample-tools", "sample-web"];

    let text = printed(&fixture, &["status", "--workspace", workspace]);
    let status: Value =
        serde_json::from_str(&printed(&fixture, &["status", "--workspace", workspace, "--format", "json"])).unwrap();
    let discovered: Value = serde_json::from_str(&printed(&fixture, &["workspace", "--workspace", workspace])).unwrap();

    assert!(text.starts_with(&format!("status of {workspace} · 3 repositories")), "{text}");
    assert_eq!(status["workspace"], workspace);
    assert_eq!(names(&status, "repos"), expected);
    assert_eq!(discovered["root"], workspace);
    assert_eq!(names(&discovered, "repos"), expected);
}

#[test]
fn a_bad_command_line_fails_with_the_reason() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let cases: [(&[&str], i32, &str); 2] = [
        (&["search", "X", "--workspace", workspace, "--repo", "nope"], 1, "uals: unknown repository \"nope\""),
        (&["search", "X", "--no-such-flag"], 2, "error: unexpected argument '--no-such-flag'"),
    ];

    for (args, code, stderr) in cases {
        let output = fixture.run(args, "", false, false);

        assert_eq!(output.status.code(), Some(code), "{args:?}");
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        assert!(stderr_text.starts_with(stderr), "{args:?}: {stderr_text}");
    }
}
