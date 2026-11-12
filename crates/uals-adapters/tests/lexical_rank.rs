//! Matching and ranking of the lexical backend (spike S3 regression test 16 and the query kinds),
//! black-box through `SearchBackend`.

mod lexical_support;

use lexical_support::*;
use uals_core::backend::BackendError;
use uals_core::config::LexicalConfig;
use uals_core::types::{BackendId, QueryKind};

#[tokio::test]
async fn identifier_ranks_declaration_then_file_name_then_exact_then_rest() {
    let dir = temp();
    write(dir.path(), "a_use.kt", "val x = Widget()\n");
    write(dir.path(), "b_substr.kt", "val y = WidgetFactory()\nval widget = 1\n");
    write(dir.path(), "Widget.kt", "package p\n// text only\n");
    write(dir.path(), "z_decl.kt", "package p\n\ndata class Widget(val a: Int)\n");
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Identifier, "p.Widget").await;

    let order: Vec<(String, u32)> =
        found.hits.iter().map(|hit| (relative(&repo, &hit.path), hit.lines.start)).collect();
    // Declaration, then the whole-word usage. `WidgetFactory` is not a whole word, and `widget`
    // differs in case, which the capital in the query rules out.
    assert_eq!(order[0], ("z_decl.kt".to_owned(), 3));
    assert_eq!(order[1], ("a_use.kt".to_owned(), 1));
    assert_eq!(order.len(), 2, "{order:?}");

    let first = &found.hits[0];
    assert!(first.is_definition);
    assert_eq!(first.symbol.as_deref(), Some("Widget"));
    assert!(first.path.is_absolute());
    assert_eq!(first.lines.start, first.lines.end);
    assert_eq!(first.snippet.as_deref(), Some("data class Widget(val a: Int)"));
    assert_eq!(first.backend, BackendId::Lexical);
    assert_eq!(first.repo, repo.name);
    assert!(!found.hits[1].is_definition && found.hits[1].symbol.is_none());
    for (position, hit) in found.hits.iter().enumerate() {
        assert_eq!(hit.rank as usize, position);
    }
}

#[tokio::test]
async fn smart_case_matches_of_other_case_rank_last() {
    let dir = temp();
    write(dir.path(), "a.kt", "val a = Widget()\n");
    write(dir.path(), "b.kt", "call(widget)\n");
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Identifier, "widget").await;
    assert_eq!(relative(&repo, &found.hits[0].path), "b.kt", "the case-exact whole word comes first");
    assert_eq!(found.hits[0].score, Some(2.0));
    assert_eq!(found.hits[1].score, Some(1.0));
}

#[tokio::test]
async fn file_name_tier_beats_other_exact_matches() {
    let dir = temp();
    write(dir.path(), "a_other.kt", "val x = Widget()\n");
    write(dir.path(), "Widget.kt", "fun make() = Widget()\n");
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Identifier, "Widget").await;
    assert_eq!(relative(&repo, &found.hits[0].path), "Widget.kt");
    assert!(!found.hits[0].is_definition);
}

#[tokio::test]
async fn declarations_are_recognized_across_languages() {
    let dir = temp();
    let lines = [
        ("k.kt", "internal interface Engine {"),
        ("o.kt", "object Engine : Base()"),
        ("e.kt", "enum class Engine { A }"),
        ("f.kt", "fun <T> Engine.ping() = 1\nfun Receiver.Engine() = 1"),
        ("p.py", "def Engine(x):"),
        ("r.rs", "pub fn Engine() {}"),
        ("s.rs", "struct Engine;"),
        ("t.rs", "trait Engine {}"),
        ("a.ts", "type Engine = string;"),
        ("b.ts", "export function Engine() {}"),
        ("c.kt", "typealias Engine = String"),
        ("d.kt", "const val Engine = 1"),
        ("g.js", "let Engine = 1;"),
    ];
    for (file, text) in lines {
        write(dir.path(), file, &format!("{text}\n"));
    }
    write(dir.path(), "usage.kt", "val z = Engine.start()\nreturn Engine\n");
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Identifier, "Engine").await;
    let definitions: Vec<String> =
        found.hits.iter().filter(|hit| hit.is_definition).map(|hit| relative(&repo, &hit.path)).collect();
    assert_eq!(definitions.len(), 14, "{definitions:?}");
    assert!(found.hits.iter().take(14).all(|hit| hit.is_definition), "definitions come first");
    assert!(!found.hits.iter().any(|hit| hit.path.ends_with("usage.kt") && hit.is_definition));
}

#[tokio::test]
async fn smart_case_applies_to_identifiers_and_literals() {
    let dir = temp();
    write(dir.path(), "a.kt", "call(widget)\ncall(Widget)\ncall(WIDGET)\n");
    let repo = repo(dir.path());
    let lower = find(&repo, QueryKind::Identifier, "widget").await;
    assert_eq!(lower.hits.len(), 3, "an all-lower query ignores case");
    let upper = find(&repo, QueryKind::Identifier, "Widget").await;
    assert_eq!(upper.hits.len(), 1, "a query with a capital is case-sensitive");
    let literal = find(&repo, QueryKind::Literal, "call(widget)").await;
    assert_eq!(literal.hits.len(), 3);
    let literal = find(&repo, QueryKind::Literal, "call(Widget)").await;
    assert_eq!(literal.hits.len(), 1);
    let literal = find(&repo, QueryKind::Literal, "CALL(").await;
    assert!(literal.hits.is_empty());
}

#[tokio::test]
async fn literal_is_a_fixed_string_not_a_regex_and_not_whole_word() {
    let dir = temp();
    write(dir.path(), "a.kt", "x = a.b(c)\nx = aXb(c)\nprefix_host_switched_suffix\n");
    let repo = repo(dir.path());
    let dotted = find(&repo, QueryKind::Literal, "a.b(c)").await;
    assert_eq!(dotted.hits.len(), 1);
    let inner = find(&repo, QueryKind::Literal, "host_switched").await;
    assert_eq!(inner.hits.len(), 1);
}

#[tokio::test]
async fn regex_matches_and_invalid_regex_fails_with_the_error() {
    let dir = temp();
    write(dir.path(), "a.kt", "class HostStore\nclass Other\nclass NetStore\n");
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Regex, r"class \w+Store").await;
    assert_eq!(found.hits.len(), 2);

    let result = try_find(&repo, &request(QueryKind::Regex, "class (")).await;
    match result {
        Err(BackendError::Failed(reason)) => assert!(reason.contains("regex"), "{reason}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn natural_ranks_by_distinct_words_and_ignores_short_ones() {
    let dir = temp();
    write(dir.path(), "a.kt", "// choose the backend host\nval backend = 1\nval host = 2\nbackend backend backend\n");
    write(dir.path(), "b.kt", "val unrelated = 1\nthe host list\n");
    let repo = repo(dir.path());
    // Words of 4+ characters: "where", "backend", "host", "chosen". `is` and `the` are dropped.
    let found = find(&repo, QueryKind::Natural, "Where is the Backend HOST chosen").await;
    let lines: Vec<(String, u32)> =
        found.hits.iter().map(|hit| (relative(&repo, &hit.path), hit.lines.start)).collect();
    // `a.kt` has 4 matching lines but only `max_hits_per_file` (3) are kept.
    assert_eq!(lines, [("a.kt".to_owned(), 1), ("a.kt".to_owned(), 2), ("a.kt".to_owned(), 3), ("b.kt".to_owned(), 2)]);
    assert_eq!(found.hits[0].score, Some(2.0), "two distinct words on the first line");
    assert_eq!(found.hits[1].score, Some(1.0));
}

#[tokio::test]
async fn natural_without_long_words_finds_nothing() {
    let dir = temp();
    write(dir.path(), "a.kt", "val a = 1\n");
    let found = find(&repo(dir.path()), QueryKind::Natural, "is it on").await;
    assert!(found.hits.is_empty() && found.partial.is_none());
}

#[tokio::test]
async fn test_paths_rank_below_main_paths_unless_the_query_asks_for_tests() {
    // 16. The test path sorts first alphabetically, so only the penalty can put it second.
    let dir = temp();
    write(dir.path(), "a/test/Thing.kt", "val s = call(zq_token)\n");
    write(dir.path(), "b/main/Thing.kt", "val s = call(zq_token)\n");
    let repo = repo(dir.path());

    let plain = find(&repo, QueryKind::Literal, "zq_token").await;
    assert_eq!(relative(&repo, &plain.hits[0].path), "b/main/Thing.kt");
    assert!(plain.hits[0].score > plain.hits[1].score);

    for query in ["zq_token test", "zq_token MOCK", "fake zq_token"] {
        let exempt = find(&repo, QueryKind::Natural, query).await;
        assert_eq!(relative(&repo, &exempt.hits[0].path), "a/test/Thing.kt", "{query}");
        assert_eq!(exempt.hits[0].score, exempt.hits[1].score, "{query}");
    }
}

#[tokio::test]
async fn the_first_matching_downrank_rule_applies() {
    let dir = temp();
    write(dir.path(), "docs/readme.md", "zq_token\n");
    write(dir.path(), "build/out.kt", "zq_token\n");
    write(dir.path(), "src/main.kt", "zq_token\n");
    let config = LexicalConfig {
        downrank: vec![
            uals_core::config::Downrank { pattern: r"^docs/".into(), factor: 0.2 },
            uals_core::config::Downrank { pattern: r"\.md$".into(), factor: 0.9 },
            uals_core::config::Downrank { pattern: r"^build/".into(), factor: 0.5 },
        ],
        ..LexicalConfig::default()
    };
    let repo = repo(dir.path());
    let found = find_with(config, &repo, QueryKind::Literal, "zq_token").await;
    let order: Vec<String> = found.hits.iter().map(|hit| relative(&repo, &hit.path)).collect();
    assert_eq!(order, ["src/main.kt", "build/out.kt", "docs/readme.md"]);
}

#[tokio::test]
async fn ties_are_broken_by_path_then_line_and_fetch_limit_applies() {
    let dir = temp();
    write(dir.path(), "b.kt", "x(zq_t)\ny(zq_t)\n");
    write(dir.path(), "a.kt", "x(zq_t)\n");
    let repo = repo(dir.path());
    let found = find(&repo, QueryKind::Literal, "zq_t").await;
    let order: Vec<(String, u32)> =
        found.hits.iter().map(|hit| (relative(&repo, &hit.path), hit.lines.start)).collect();
    assert_eq!(order, [("a.kt".to_owned(), 1), ("b.kt".to_owned(), 1), ("b.kt".to_owned(), 2)]);

    let mut limited = request(QueryKind::Literal, "zq_t");
    limited.fetch_limit = 2;
    let answer = try_find(&repo, &limited).await.expect("ok");
    assert_eq!(answer.hits.len(), 2);
    assert_eq!(answer.hits[1].rank, 1);
}

#[tokio::test]
async fn fixture_workspace_finds_the_definition_first() {
    let dir = temp();
    let workspace = fixture_copy(dir.path());
    let repo = repo(&workspace.join("sample-android"));

    let found = find(&repo, QueryKind::Identifier, "sample.core.Analytics").await;
    let first = &found.hits[0];
    assert_eq!(relative(&repo, &first.path), "core/src/main/kotlin/sample/core/Analytics.kt");
    assert!(first.is_definition);
    assert_eq!(first.symbol.as_deref(), Some("Analytics"));

    let found = find(&repo, QueryKind::Identifier, "RetryPolicy").await;
    assert_eq!(relative(&repo, &found.hits[0].path), "core/src/main/kotlin/sample/core/RetryPolicy.kt");
    assert!(found.hits[0].is_definition);
    let test_hit = found.hits.iter().position(|hit| hit.path.ends_with("RetryPolicyTest.kt")).expect("test file hit");
    let main_use = found.hits.iter().position(|hit| hit.path.ends_with("App.kt")).expect("usage hit");
    assert!(main_use < test_hit, "the test file ranks below main sources");

    let found = find(&repo, QueryKind::Identifier, "HostSelector").await;
    assert!(found.hits[0].is_definition && found.hits[0].path.ends_with("HostSelector.kt"));
}
