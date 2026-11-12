//! The `uals hook post-edit` and `uals refresh` commands with a fake ast-index: a `sh` script that
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
use serde_json::json;
use tempfile::TempDir;

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
        printed.starts_with("sample-web: ast-index failed (ast-index update exited with Some(1): disk full)"),
        "{printed}"
    );
    assert_eq!(working_refresh.status.code(), Some(0), "{}", String::from_utf8_lossy(&working_refresh.stdout));
}
