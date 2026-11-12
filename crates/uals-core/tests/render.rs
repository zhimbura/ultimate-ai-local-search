//! Render tests on hand-built responses, compared with exact text.

use pretty_assertions::assert_eq;
use uals_core::config::OutputConfig;
use uals_core::render::{Format, page, render_search, render_usages, snippet_range};
use uals_core::types::{
    BackendId, BackendReport, BackendStatus, FusedHit, LineRange, QueryKind, RepoUsages, SearchResponse, Snippet,
    SourceRef, SymbolKind, Usage, UsagesResponse,
};

const HOST_SELECTOR: &str = "/ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt";

fn fused(repo: &str, path: &str, lines: (u32, u32), focus: u32) -> FusedHit {
    FusedHit {
        repo: repo.into(),
        path: path.into(),
        lines: LineRange::new(lines.0, lines.1),
        symbol: None,
        symbol_kind: None,
        is_definition: false,
        pinned: false,
        focus_line: focus,
        snippet: None,
        score: 0.01639344262295082,
        sources: vec![SourceRef { backend: BackendId::Semble, rank: 0 }],
    }
}

fn snippet(start: u32, lines: &[&str]) -> Snippet {
    let end = start + lines.len() as u32 - 1;
    Snippet { lines: LineRange::new(start, end), text: lines.join("\n") }
}

fn three_hits() -> SearchResponse {
    let mut first = fused("sample-android", HOST_SELECTOR, (3, 30), 3);
    first.symbol = Some("HostSelector".into());
    first.symbol_kind = Some(SymbolKind::Class);
    first.is_definition = true;
    first.pinned = true;
    first.score = 0.0278688;
    first.sources = vec![
        SourceRef { backend: BackendId::AstIndex, rank: 0 },
        SourceRef { backend: BackendId::Lexical, rank: 0 },
        SourceRef { backend: BackendId::Semble, rank: 1 },
    ];
    first.snippet = Some(snippet(8, &["class HostSelector(", "    private val store: HostStore,", "", "x"]));

    let mut second = fused("sample-web", "/ws/sample-web/src/hosts.ts", (1, 12), 5);
    second.symbol = Some("pickHost".into());
    second.snippet = Some(snippet(9, &["export function pickHost() {", "  return 1", "}"]));

    let third = fused("sample-web", "/ws/sample-web/src/other.ts", (40, 41), 40);

    let report =
        |backend, repo: &str, status, elapsed_ms| BackendReport { backend, repo: repo.into(), status, elapsed_ms };
    SearchResponse {
        query: "HostSelector".into(),
        kind: QueryKind::Identifier,
        total: 3,
        offset: 0,
        hits: vec![first, second, third],
        next_offset: None,
        backends: vec![
            report(BackendId::AstIndex, "sample-android", BackendStatus::Ok, 12),
            report(BackendId::Semble, "sample-android", BackendStatus::Timeout, 4000),
            report(BackendId::Lexical, "sample-web", BackendStatus::Unavailable { reason: "no index".into() }, 0),
            report(BackendId::AstIndex, "sample-web", BackendStatus::Ok, 7),
        ],
    }
}

#[test]
fn concise_search_layout() {
    let expected = "\
search \"HostSelector\" · identifier · 1-3 of 3
1. sample-android · /ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt:3-30 · class HostSelector · definition · pinned · ast-index#1 lexical#1 semble#2
     8 | class HostSelector(
     9 |     private val store: HostStore,
    10 |
    11 | x
2. sample-web · /ws/sample-web/src/hosts.ts:1-12 · pickHost · semble#1
     9 | export function pickHost() {
    10 |   return 1
    11 | }
3. sample-web · /ws/sample-web/src/other.ts:40-41 · semble#1
backends: sample-android: ast-index ok 12ms, semble timeout 4000ms; sample-web: lexical unavailable (no index) 0ms, ast-index ok 7ms";
    assert_eq!(render_search(&three_hits(), Format::Concise, &OutputConfig::default()), expected);
}

#[test]
fn detailed_search_adds_the_score() {
    let mut response = three_hits();
    response.hits.truncate(1);
    response.backends.clear();
    response.total = 9;
    response.offset = 4;
    response.next_offset = Some(5);
    let expected = "\
search \"HostSelector\" · identifier · 5-5 of 9
5. sample-android · /ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt:3-30 · class HostSelector · definition · pinned · score 0.02787 · ast-index#1 lexical#1 semble#2
     8 | class HostSelector(
     9 |     private val store: HostStore,
    10 |
    11 | x
next: offset=5";
    assert_eq!(render_search(&response, Format::Detailed, &OutputConfig::default()), expected);
}

#[test]
fn empty_search_has_a_header_only() {
    let response = SearchResponse {
        query: "nothing".into(),
        kind: QueryKind::Natural,
        total: 0,
        offset: 0,
        hits: Vec::new(),
        next_offset: None,
        backends: Vec::new(),
    };
    assert_eq!(
        render_search(&response, Format::Concise, &OutputConfig::default()),
        "search \"nothing\" · natural · 0 of 0"
    );
}

#[test]
fn long_lines_are_cut_after_200_characters() {
    let mut response = three_hits();
    response.hits.truncate(1);
    response.backends.clear();
    let long = "é".repeat(201);
    let exact = "a".repeat(200);
    response.hits[0].snippet = Some(snippet(1, &[long.as_str(), exact.as_str()]));
    let text = render_search(&response, Format::Concise, &OutputConfig::default());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[2], format!("    1 | {}…", "é".repeat(200)));
    assert_eq!(lines[3], format!("    2 | {exact}"));
}

#[test]
fn budget_drops_snippets_from_the_last_hit_upwards() {
    let response = three_hits();
    let full = render_search(&response, Format::Concise, &OutputConfig::default());
    // Without the last snippet nothing changes (it has none); without the second one it fits here.
    let without_second = full.replace("     9 | export function pickHost() {\n    10 |   return 1\n    11 | }\n", "");
    let config = OutputConfig { max_output_chars: without_second.chars().count(), ..OutputConfig::default() };
    let text = render_search(&response, Format::Concise, &config);
    assert_eq!(text, without_second);
    assert!(text.contains("     8 | class HostSelector("));
}

#[test]
fn budget_then_drops_hits_and_reports_what_was_printed() {
    let response = three_hits();
    let expected = "\
search \"HostSelector\" · identifier · 1-2 of 3
1. sample-android · /ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt:3-30 · class HostSelector · definition · pinned · ast-index#1 lexical#1 semble#2
2. sample-web · /ws/sample-web/src/hosts.ts:1-12 · pickHost · semble#1
backends: sample-android: ast-index ok 12ms, semble timeout 4000ms; sample-web: lexical unavailable (no index) 0ms, ast-index ok 7ms
next: offset=2";
    // The limit is exactly the size of the two-hit text, so the third hit is what does not fit.
    let config = OutputConfig { max_output_chars: expected.chars().count(), ..OutputConfig::default() };
    let text = render_search(&response, Format::Concise, &config);
    assert_eq!(text, expected);
}

#[test]
fn budget_keeps_the_first_hit_even_when_it_cannot_fit() {
    let response = three_hits();
    let config = OutputConfig { max_output_chars: 10, ..OutputConfig::default() };
    let text = render_search(&response, Format::Concise, &config);
    assert!(text.starts_with("search \"HostSelector\" · identifier · 1-1 of 3\n1. sample-android"));
    assert!(text.ends_with("next: offset=1"));
    assert!(!text.contains(" | "));
}

#[test]
fn json_search_is_compact_and_trimmed_by_the_same_rule() {
    let mut response = three_hits();
    response.backends.clear();
    let full = render_search(&response, Format::Json, &OutputConfig::default());
    assert!(!full.contains('\n'));
    let parsed: SearchResponse = serde_json::from_str(&full).unwrap();
    assert_eq!(parsed, response);

    let config = OutputConfig { max_output_chars: full.chars().count() - 1, ..OutputConfig::default() };
    let trimmed: SearchResponse = serde_json::from_str(&render_search(&response, Format::Json, &config)).unwrap();
    assert_eq!(trimmed.hits.len(), 3);
    assert!(trimmed.hits[1].snippet.is_none(), "the last snippet goes first");
    assert!(trimmed.hits[0].snippet.is_some());

    let config = OutputConfig { max_output_chars: 600, ..OutputConfig::default() };
    let trimmed: SearchResponse = serde_json::from_str(&render_search(&response, Format::Json, &config)).unwrap();
    assert_eq!((trimmed.hits.len(), trimmed.next_offset, trimmed.total), (1, Some(1), 3));
}

#[test]
fn snippet_range_centers_on_the_focus() {
    let config = OutputConfig { max_snippet_lines: 5, ..OutputConfig::default() };
    let hit = |lines: (u32, u32), focus| fused("r", "/ws/r/a.kt", lines, focus);
    let range = |hit: &FusedHit, format| {
        let r = snippet_range(hit, format, &config);
        (r.start, r.end)
    };
    assert_eq!(range(&hit((1, 5), 3), Format::Concise), (1, 5), "fits whole");
    assert_eq!(range(&hit((10, 100), 50), Format::Concise), (48, 52));
    assert_eq!(range(&hit((10, 100), 10), Format::Concise), (10, 14), "near the start");
    assert_eq!(range(&hit((10, 100), 100), Format::Concise), (96, 100), "near the end");
    assert_eq!(range(&hit((10, 100), 11), Format::Concise), (10, 14));
    assert_eq!(range(&hit((10, 100), 99), Format::Concise), (96, 100));
    assert_eq!(range(&hit((10, 100), 50), Format::Json), (48, 52));
    assert_eq!(range(&hit((10, 100), 50), Format::Detailed), (45, 54));
    assert_eq!(range(&hit((10, 14), 12), Format::Detailed), (10, 14));
    assert_eq!(range(&hit((10, 100), 3), Format::Concise), (10, 14), "focus outside is clamped");
    assert_eq!(range(&hit((1, u32::MAX), u32::MAX), Format::Concise), (u32::MAX - 4, u32::MAX));
}

#[test]
fn snippet_range_with_zero_lines_still_shows_the_focus() {
    let config = OutputConfig { max_snippet_lines: 0, ..OutputConfig::default() };
    let hit = fused("r", "/ws/r/a.kt", (10, 20), 15);
    assert_eq!(snippet_range(&hit, Format::Concise, &config), LineRange::line(15));
}

#[test]
fn page_bounds() {
    let items = || vec![1, 2, 3, 4, 5];
    assert_eq!(page(items(), 0, 2), (vec![1, 2], Some(2)));
    assert_eq!(page(items(), 2, 2), (vec![3, 4], Some(4)));
    assert_eq!(page(items(), 4, 2), (vec![5], None));
    assert_eq!(page(items(), 0, 5), (vec![1, 2, 3, 4, 5], None));
    assert_eq!(page(items(), 0, 100), (vec![1, 2, 3, 4, 5], None));
    assert_eq!(page(items(), 5, 2), (Vec::<i32>::new(), None));
    assert_eq!(page(items(), 99, 2), (Vec::<i32>::new(), None));
    assert_eq!(page(items(), 1, usize::MAX), (vec![2, 3, 4, 5], None));
    assert_eq!(page(items(), usize::MAX, 2), (Vec::<i32>::new(), None));
    assert_eq!(page(items(), 1, 0), (Vec::<i32>::new(), Some(1)));
    assert_eq!(page(Vec::<i32>::new(), 0, 3), (Vec::<i32>::new(), None));
}

fn usage(repo: &str, path: &str, line: u32, context: Option<&str>) -> Usage {
    Usage { repo: repo.into(), path: path.into(), line, context: context.map(Into::into) }
}

fn usages_response() -> UsagesResponse {
    UsagesResponse {
        symbol: "HostSelector".into(),
        total: 5,
        offset: 0,
        repos: vec![
            RepoUsages {
                repo: "sample-android".into(),
                source: BackendId::AstIndex,
                status: BackendStatus::Ok,
                usages: vec![
                    usage("sample-android", "/ws/sample-android/app/App.kt", 9, Some("val s = HostSelector()")),
                    usage("sample-android", "/ws/sample-android/app/App.kt", 14, None),
                    usage("sample-android", "/ws/sample-android/app/Main.kt", 3, None),
                ],
            },
            RepoUsages {
                repo: "sample-web".into(),
                source: BackendId::Lexical,
                status: BackendStatus::Unavailable { reason: "ast-index unavailable (no index)".into() },
                usages: vec![
                    usage("sample-web", "/ws/sample-web/a.ts", 1, None),
                    usage("sample-web", "/ws/sample-web/b.ts", 2, None),
                ],
            },
        ],
        next_offset: None,
        complete: true,
    }
}

#[test]
fn concise_usages_layout() {
    let expected = "\
usages of \"HostSelector\" · 5 in 4 files · complete
sample-android (ast-index)
  /ws/sample-android/app/App.kt: 9, 14
  /ws/sample-android/app/Main.kt: 3
sample-web (lexical: ast-index unavailable (no index))
  /ws/sample-web/a.ts: 1
  /ws/sample-web/b.ts: 2";
    assert_eq!(render_usages(&usages_response(), Format::Concise, &OutputConfig::default()), expected);
}

#[test]
fn detailed_usages_show_context_and_incomplete() {
    let mut response = usages_response();
    response.repos.truncate(1);
    response.total = 3;
    response.complete = false;
    response.next_offset = None;
    let expected = "\
usages of \"HostSelector\" · 3 in 2 files · incomplete
sample-android (ast-index)
  /ws/sample-android/app/App.kt
    9: val s = HostSelector()
    14:
  /ws/sample-android/app/Main.kt
    3:";
    assert_eq!(render_usages(&response, Format::Detailed, &OutputConfig::default()), expected);
}

#[test]
fn usages_budget_keeps_a_prefix_and_moves_next_offset() {
    let response = usages_response();
    let config = OutputConfig { max_output_chars: 190, ..OutputConfig::default() };
    let text = render_usages(&response, Format::Concise, &config);
    let expected = "\
usages of \"HostSelector\" · 3 of 5 in 2 files · complete
sample-android (ast-index)
  /ws/sample-android/app/App.kt: 9, 14
  /ws/sample-android/app/Main.kt: 3
next: offset=3";
    assert!(text.chars().count() <= 190);
    assert_eq!(text, expected);

    let tiny = OutputConfig { max_output_chars: 5, ..OutputConfig::default() };
    let text = render_usages(&response, Format::Concise, &tiny);
    assert!(text.starts_with("usages of \"HostSelector\" · 1 of 5 in 1 files"));
    assert!(text.ends_with("next: offset=1"));
}

#[test]
fn json_usages_round_trip() {
    let response = usages_response();
    let text = render_usages(&response, Format::Json, &OutputConfig::default());
    assert_eq!(serde_json::from_str::<UsagesResponse>(&text).unwrap(), response);
}

#[test]
fn empty_usages() {
    let response = UsagesResponse {
        symbol: "Nope".into(),
        total: 0,
        offset: 0,
        repos: vec![RepoUsages {
            repo: "r".into(),
            source: BackendId::Lexical,
            status: BackendStatus::Ok,
            usages: Vec::new(),
        }],
        next_offset: None,
        complete: true,
    };
    assert_eq!(
        render_usages(&response, Format::Concise, &OutputConfig::default()),
        "usages of \"Nope\" · 0 in 0 files · complete\nr (lexical)"
    );
}
