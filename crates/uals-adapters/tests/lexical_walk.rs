//! Walk behavior of the lexical backend (spike S3 regression tests 1-5, 7-9, 11-12, 14-15, 17),
//! black-box through `SearchBackend` and `UsageSource`.

mod lexical_support;

use std::fs;
use std::process::Command;
use std::time::Duration;

use lexical_support::*;
use uals_adapters::lexical::LexicalBackend;
use uals_core::backend::{BackendError, SearchBackend, UsageSource};
use uals_core::config::LexicalConfig;
use uals_core::types::{QueryKind, Repo};

const NEEDLE: &str = "zq_needle";

fn mark(root: &std::path::Path, relative: &str) {
    write(root, relative, &format!("val {NEEDLE} = 1\n"));
}

#[tokio::test]
async fn non_git_parent_honors_child_gitignore() {
    // 1. The parent is not a repository; each child is one and has its own ignore file.
    let dir = temp();
    let parent = dir.path().join("ws");
    for child in ["one", "two"] {
        let root = parent.join(child);
        git_init(&root);
        write(&root, ".gitignore", "build/\nnode_modules/\n");
        mark(&root, "src/main.kt");
        mark(&root, "build/gen/Out.kt");
        mark(&root, "node_modules/x/index.ts");
    }
    let found = find(&repo(&parent), QueryKind::Literal, NEEDLE).await;
    assert_eq!(files(&repo(&parent), &found), ["one/src/main.kt", "two/src/main.kt"]);
}

#[tokio::test]
async fn gitignore_is_honored_without_dot_git() {
    // 2. An exported copy: `.gitignore` but no `.git`.
    let dir = temp();
    write(dir.path(), ".gitignore", "build/\n");
    mark(dir.path(), "src/main.kt");
    mark(dir.path(), "build/gen/Out.kt");
    let repo = repo(dir.path());
    assert_eq!(files(&repo, &find(&repo, QueryKind::Literal, NEEDLE).await), ["src/main.kt"]);
}

#[tokio::test]
async fn ignore_files_above_the_root_are_not_applied() {
    // 3. A stray `.gitignore` and `.ignore` above the root must not hide the repository.
    let dir = temp();
    write(dir.path(), ".gitignore", "inner/\n*.kt\n");
    write(dir.path(), ".ignore", "inner/\n");
    let inner = dir.path().join("inner");
    git_init(&inner);
    write(&inner, ".gitignore", "skipped.kt\n");
    mark(&inner, "kept.kt");
    mark(&inner, "skipped.kt");
    let repo = repo(&inner);
    assert_eq!(files(&repo, &find(&repo, QueryKind::Literal, NEEDLE).await), ["kept.kt"]);
}

#[tokio::test]
async fn dot_git_directory_and_file_are_never_searched() {
    // 4. `.git` as a directory (repository) and as a file (worktree, submodule).
    let dir = temp();
    git_init(dir.path());
    write(dir.path(), ".git/notes.txt", &format!("{NEEDLE}\n"));
    write(dir.path(), "linked/.git", &format!("gitdir: /elsewhere/{NEEDLE}\n"));
    mark(dir.path(), "linked/code.kt");
    let repo = repo(dir.path());
    assert_eq!(files(&repo, &find(&repo, QueryKind::Literal, NEEDLE).await), ["linked/code.kt"]);
}

#[tokio::test]
async fn hidden_files_are_searched_and_ds_store_is_not() {
    // 5.
    let dir = temp();
    git_init(dir.path());
    mark(dir.path(), ".github/workflows/ci.yml");
    mark(dir.path(), ".hidden-config");
    mark(dir.path(), "sub/.DS_Store");
    write(dir.path(), ".gitignore", &format!("# {NEEDLE}\n"));
    let repo = repo(dir.path());
    assert_eq!(
        files(&repo, &find(&repo, QueryKind::Literal, NEEDLE).await),
        [".github/workflows/ci.yml", ".gitignore", ".hidden-config"]
    );
}

#[tokio::test]
async fn searched_files_equal_the_git_file_list_except_known_differences() {
    // 7. Fixture projects with junk: the searched set is `git ls-files` plus untracked, not
    // ignored files; tracked symlinks and tracked-but-ignored files are the known differences.
    let dir = temp();
    let workspace = fixture_copy(dir.path());
    let project = workspace.join("sample-web");
    write(&project, ".gitignore", "build/\nnode_modules/\nforced.ts\n");
    mark(&project, "build/gen/X.ts");
    mark(&project, "node_modules/x/index.ts");
    mark(&project, "untracked.ts");
    mark(&project, "forced.ts");
    #[cfg(unix)]
    std::os::unix::fs::symlink("src", project.join("link-to-src")).expect("symlink");
    git(&project, &["add", "-A"]);
    git(&project, &["add", "-f", "forced.ts"]);
    git(&project, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "init"]);
    mark(&project, "later-untracked.ts");

    let listed = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["ls-files"];
        args.extend_from_slice(extra);
        let out = Command::new("git").args(args).current_dir(&project).output().expect("git ls-files");
        String::from_utf8(out.stdout).expect("utf8").lines().map(str::to_owned).collect()
    };
    let mut expected = listed(&[]);
    expected.extend(listed(&["-o", "--exclude-standard"]));
    expected.retain(|path| path != "link-to-src" && path != "forced.ts");
    expected.sort();
    expected.dedup();

    let repo = repo(&project);
    // `^` matches in every non-empty file; the per-file cap of 1 keeps one hit per file.
    let config = LexicalConfig { max_matches_per_file: 1, max_candidates: 100_000, ..LexicalConfig::default() };
    let found = find_with(config, &repo, QueryKind::Regex, "^").await;
    assert_eq!(files(&repo, &found), expected);
    assert!(found.partial.is_none());
}

/// Name of the child test that runs with a private `HOME`.
const GLOBAL_CHILD: &str = "global_gitignore_child";

#[test]
fn global_gitignore_is_honored_with_an_isolated_home() {
    // 8. The parent test starts this binary again with `HOME` and `XDG_CONFIG_HOME` pointing at
    // a temp folder that holds a global ignore file; the user's own git config never matters.
    let home = temp();
    write(home.path(), "xdg/git/ignore", "*.globalignored\n");
    let status = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", GLOBAL_CHILD, "--nocapture", "--test-threads=1"])
        .env("UALS_TEST_GLOBAL_CHILD", "1")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("xdg"))
        .env_remove("GIT_CONFIG_GLOBAL")
        .status()
        .expect("start the child test");
    assert!(status.success(), "the child test failed");
}

#[tokio::test]
async fn global_gitignore_child() {
    if std::env::var_os("UALS_TEST_GLOBAL_CHILD").is_none() {
        return;
    }
    let dir = temp();
    git_init(dir.path());
    mark(dir.path(), "a.kt");
    mark(dir.path(), "b.globalignored");
    let repo = repo(dir.path());
    assert_eq!(files(&repo, &find(&repo, QueryKind::Literal, NEEDLE).await), ["a.kt"]);
}

#[tokio::test]
async fn binary_files_are_dropped_but_late_nul_keeps_earlier_matches() {
    // 9. A NUL in the first buffer drops the file; a NUL after it keeps the earlier matches;
    // line numbers do not shift.
    let dir = temp();
    write(dir.path(), "text.txt", &format!("one\n{NEEDLE}\n"));
    write_bytes(dir.path(), "early_nul.bin", format!("\0\n{NEEDLE}\n").as_bytes());
    let mut late = format!("one\n{NEEDLE}\n").into_bytes();
    late.extend(std::iter::repeat_n(b'x', 300_000));
    late.extend_from_slice(b"\n\0\n");
    write_bytes(dir.path(), "late_nul.bin", &late);
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Literal, NEEDLE).await;
    assert_eq!(files(&repo, &found), ["late_nul.bin", "text.txt"]);
    assert!(found.hits.iter().all(|hit| hit.lines.start == 2), "line numbers must stay put");
}

#[tokio::test]
async fn latin1_and_utf16_files_match_and_output_is_lossy() {
    // 10.
    let dir = temp();
    write_bytes(dir.path(), "latin1.txt", b"caf\xe9 zq_needle\n");
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "zq_needle in utf16\n".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    write_bytes(dir.path(), "utf16.txt", &utf16);
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Literal, NEEDLE).await;
    assert_eq!(files(&repo, &found), ["latin1.txt", "utf16.txt"]);
    let latin1 = found.hits.iter().find(|hit| hit.path.ends_with("latin1.txt")).expect("latin1 hit");
    assert_eq!(latin1.snippet.as_deref(), Some("caf\u{fffd} zq_needle"));
}

#[tokio::test]
async fn huge_single_line_file_is_skipped_and_the_query_still_succeeds() {
    // 11. Over `max_file_size_bytes`: skipped. With the cap lifted the line exceeds the heap
    // limit of the searcher: a per-file error, logged and skipped.
    let dir = temp();
    mark(dir.path(), "small.kt");
    let mut huge = format!("{NEEDLE} ").into_bytes();
    huge.extend(std::iter::repeat_n(b'x', 24 * 1024 * 1024));
    write_bytes(dir.path(), "huge.txt", &huge);
    let repo = repo(dir.path());

    let found = find(&repo, QueryKind::Literal, NEEDLE).await;
    assert_eq!(files(&repo, &found), ["small.kt"]);

    let lifted = LexicalConfig { max_file_size_bytes: 64 * 1024 * 1024, ..LexicalConfig::default() };
    let found = find_with(lifted, &repo, QueryKind::Literal, NEEDLE).await;
    assert!(files(&repo, &found).contains(&"small.kt".to_owned()));
    assert!(found.hits.iter().all(|hit| hit.snippet.as_deref().is_none_or(|text| text.chars().count() <= 300)));
}

#[tokio::test]
async fn per_file_and_candidate_caps_are_honored() {
    // 12.
    let dir = temp();
    let many: String = (0..50).map(|i| format!("call({NEEDLE}, {i})\n")).collect();
    for file in 0..20 {
        write(dir.path(), &format!("src/f{file:02}.kt"), &many);
    }
    let repo = repo(dir.path());

    let per_file =
        LexicalConfig { max_matches_per_file: 5, max_hits_per_file: 2, max_candidates: 10_000, ..Default::default() };
    let found = find_with(per_file, &repo, QueryKind::Literal, NEEDLE).await;
    assert_eq!(found.hits.len(), 20 * 2);
    assert!(found.partial.is_none());

    let capped =
        LexicalConfig { max_matches_per_file: 5, max_hits_per_file: 5, max_candidates: 7, ..Default::default() };
    let found = find_with(capped, &repo, QueryKind::Literal, NEEDLE).await;
    assert!(found.hits.len() <= 7 && !found.hits.is_empty());
    assert!(found.partial.as_deref().is_some_and(|reason| reason.contains("candidate cap")));
    for (position, hit) in found.hits.iter().enumerate() {
        assert_eq!(hit.rank as usize, position);
    }
}

#[tokio::test]
async fn exactly_as_many_matches_as_the_cap_is_not_partial() {
    let dir = temp();
    for file in 0..5 {
        mark(dir.path(), &format!("f{file}.kt"));
    }
    let config = LexicalConfig { max_candidates: 5, ..Default::default() };
    let found = find_with(config, &repo(dir.path()), QueryKind::Literal, NEEDLE).await;
    assert_eq!(found.hits.len(), 5);
    assert!(found.partial.is_none());
}

#[tokio::test]
async fn stored_lines_are_cut_to_max_line_chars() {
    // 12, last part. Multi-byte characters must not be split.
    let dir = temp();
    write(dir.path(), "long.kt", &format!("{NEEDLE} {}\n", "ы".repeat(1000)));
    let config = LexicalConfig { max_line_chars: 50, ..Default::default() };
    let found = find_with(config, &repo(dir.path()), QueryKind::Literal, NEEDLE).await;
    let snippet = found.hits[0].snippet.as_deref().expect("snippet");
    assert_eq!(snippet.chars().count(), 50);
    assert!(snippet.starts_with(NEEDLE));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_not_followed_and_nothing_outside_is_read() {
    // 14. A link to a file and to a directory outside the repository, links inside it, a loop.
    let outside = temp();
    mark(outside.path(), "secret.kt");
    mark(outside.path(), "dir/inner.kt");
    let dir = temp();
    mark(dir.path(), "real.kt");
    mark(dir.path(), "sub/also.kt");
    std::os::unix::fs::symlink(outside.path().join("secret.kt"), dir.path().join("link_file.kt")).expect("link");
    std::os::unix::fs::symlink(outside.path().join("dir"), dir.path().join("link_dir")).expect("link");
    std::os::unix::fs::symlink(dir.path().join("real.kt"), dir.path().join("link_inside.kt")).expect("link");
    std::os::unix::fs::symlink(dir.path(), dir.path().join("sub/loop")).expect("link");
    let repo = repo(dir.path());
    assert_eq!(files(&repo, &find(&repo, QueryKind::Literal, NEEDLE).await), ["real.kt", "sub/also.kt"]);
}

#[tokio::test]
async fn ignore_and_rgignore_files_are_honored() {
    // 15, first part.
    let dir = temp();
    mark(dir.path(), "kept.kt");
    mark(dir.path(), "by_ignore.kt");
    mark(dir.path(), "by_rgignore.kt");
    write(dir.path(), ".ignore", "by_ignore.kt\n");
    write(dir.path(), ".rgignore", "by_rgignore.kt\n");
    let repo = repo(dir.path());
    assert_eq!(files(&repo, &find(&repo, QueryKind::Literal, NEEDLE).await), ["kept.kt"]);
}

#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_file_is_skipped() {
    // 15, second part. (Skipped when the process can read anything, e.g. runs as root.)
    use std::os::unix::fs::PermissionsExt;
    let dir = temp();
    mark(dir.path(), "ok.kt");
    mark(dir.path(), "locked.kt");
    let locked = dir.path().join("locked.kt");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Literal, NEEDLE).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("chmod back");
    let files = files(&repo, &found);
    assert!(files == ["ok.kt"] || files == ["locked.kt", "ok.kt"], "unexpected files: {files:?}");
}

#[tokio::test]
async fn unbounded_query_stays_bounded() {
    // 17. Query `.` matches every line; the caps bound what is kept.
    let dir = temp();
    let text: String = (0..200).map(|i| format!("line number {i} with some text\n")).collect();
    for file in 0..60 {
        write(dir.path(), &format!("d{}/f{file}.txt", file % 5), &text);
    }
    let config = LexicalConfig { max_matches_per_file: 20, max_candidates: 100, ..Default::default() };
    let repo = repo(dir.path());
    let found = find_with(config, &repo, QueryKind::Regex, ".").await;
    assert!(found.hits.len() <= 100);
    assert!(found.partial.is_some());
    let kept_per_file = |name: &str| found.hits.iter().filter(|hit| hit.path.ends_with(name)).count();
    assert!(kept_per_file("f0.txt") <= LexicalConfig::default().max_hits_per_file);
}

#[tokio::test]
async fn paths_restrict_the_walk_and_never_resurrect_ignored_files() {
    let dir = temp();
    write(dir.path(), ".gitignore", "build/\n");
    mark(dir.path(), "src/main/A.kt");
    mark(dir.path(), "src/main/build/Gen.kt");
    mark(dir.path(), "src/test/B.kt");
    mark(dir.path(), "docs/C.md");
    let repo = repo(dir.path());
    let mut request = request(QueryKind::Literal, NEEDLE);

    request.paths = vec!["src/main/**".to_owned()];
    assert_eq!(files(&repo, &try_find(&repo, &request).await.expect("ok")), ["src/main/A.kt"]);

    request.paths = vec!["*.md".to_owned(), "src/test".to_owned()];
    assert_eq!(files(&repo, &try_find(&repo, &request).await.expect("ok")), ["docs/C.md", "src/test/B.kt"]);
}

#[tokio::test]
async fn path_negations_deselect_inside_a_selected_folder_and_alone_start_from_everything() {
    let dir = temp();
    mark(dir.path(), "app/src/A.kt");
    mark(dir.path(), "app/test/B.kt");
    mark(dir.path(), "lib/C.kt");
    let repo = repo(dir.path());
    let mut request = request(QueryKind::Literal, NEEDLE);

    request.paths = vec!["app".to_owned(), "!app/test".to_owned()];
    assert_eq!(files(&repo, &try_find(&repo, &request).await.expect("ok")), ["app/src/A.kt"]);

    request.paths = vec!["!app/test".to_owned()];
    assert_eq!(files(&repo, &try_find(&repo, &request).await.expect("ok")), ["app/src/A.kt", "lib/C.kt"]);
}

#[tokio::test]
async fn a_missing_root_is_unavailable() {
    let missing = Repo { name: "gone".to_owned(), root: "/definitely/not/here".into() };
    let result = backend().search(&missing, &request(QueryKind::Literal, NEEDLE)).await;
    assert!(matches!(result, Err(BackendError::Unavailable(_))), "{result:?}");
}

#[tokio::test]
async fn index_state_needs_no_index_and_walks_nothing() {
    let missing = Repo { name: "gone".to_owned(), root: "/definitely/not/here".into() };
    let state = backend().index_state(&missing).await;
    assert!(state.indexed);
    assert_eq!(state.detail.as_deref(), Some("no index needed"));
    assert_eq!(state.repo, "gone");
    assert!(state.stale.is_none() && state.updated_at_ms.is_none());
}

#[tokio::test]
async fn usages_are_whole_word_case_sensitive_and_skip_declarations() {
    let dir = temp();
    write(dir.path(), "Retry.kt", "class Retry(val n: Int)\n");
    write(dir.path(), "use.kt", "val a = Retry(1)\nval b = RetryPolicy()\nval c = retry(2)\n  Retry.go()\n");
    let repo = repo(dir.path());
    let answer = UsageSource::usages(&backend(), &repo, "pkg.Retry", 100).await.expect("usages");
    assert!(answer.complete);
    let lines: Vec<(String, u32)> =
        answer.usages.iter().map(|usage| (relative(&repo, &usage.path), usage.line)).collect();
    assert_eq!(lines, [("use.kt".to_owned(), 1), ("use.kt".to_owned(), 4)]);
    assert_eq!(answer.usages[0].context.as_deref(), Some("val a = Retry(1)"));
    assert_eq!(answer.usages[1].context.as_deref(), Some("Retry.go()"));
    assert_eq!(answer.usages[0].repo, repo.name);
}

#[tokio::test]
async fn usages_report_incomplete_when_the_limit_cuts() {
    let dir = temp();
    for file in 0..6 {
        write(dir.path(), &format!("u{file}.kt"), "call(Target)\n");
    }
    let repo = repo(dir.path());
    let cut = UsageSource::usages(&backend(), &repo, "Target", 4).await.expect("usages");
    assert_eq!(cut.usages.len(), 4);
    assert!(!cut.complete);
    let exact = UsageSource::usages(&backend(), &repo, "Target", 6).await.expect("usages");
    assert_eq!(exact.usages.len(), 6);
    assert!(exact.complete);
}

#[tokio::test]
async fn usages_report_incomplete_at_the_deadline() {
    let dir = temp();
    mark(dir.path(), "a.kt");
    let backend = LexicalBackend::new(LexicalConfig::default(), Duration::ZERO);
    let answer = UsageSource::usages(&backend, &repo(dir.path()), NEEDLE, 10).await.expect("usages");
    assert!(!answer.complete);
}

#[tokio::test]
async fn deadline_returns_a_partial_answer_without_hanging() {
    // 13.
    let dir = temp();
    for file in 0..200 {
        mark(dir.path(), &format!("d{}/f{file}.kt", file % 10));
    }
    let repo = repo(dir.path());
    let hurried = LexicalBackend::new(LexicalConfig::default(), Duration::ZERO);
    let found = hurried.search(&repo, &request(QueryKind::Literal, NEEDLE)).await.expect("search succeeds");
    assert!(found.partial.as_deref().is_some_and(|reason| reason.contains("deadline")), "{:?}", found.partial);

    let relaxed = find(&repo, QueryKind::Literal, NEEDLE).await;
    assert_eq!(relaxed.hits.len(), 200);
    assert!(relaxed.partial.is_none());
}
