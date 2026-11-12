//! Golden cases for the pure pipeline: `tests/golden/classify.json` and `tests/golden/fusion/*.json`.
//! They are the contract for track T6. Change them only together with the coordinator, and never
//! to make an implementation pass.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use pretty_assertions::assert_eq;
use serde::Deserialize;
use uals_core::classify::classify;
use uals_core::config::FusionConfig;
use uals_core::fuse::fuse;
use uals_core::types::{BackendId, FusedHit, Hit, QueryKind, SymbolKind, SymbolSpan};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifyCase {
    query: String,
    #[serde(default)]
    forced: Option<QueryKind>,
    kind: QueryKind,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FusionCase {
    name: String,
    description: String,
    query: String,
    kind: QueryKind,
    spans: Vec<SymbolSpan>,
    hits: Vec<Hit>,
    expected: Vec<Row>,
}

/// One fused hit in a comparable form. Scores are compared at 9 decimals.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    repo: String,
    path: PathBuf,
    lines: [u32; 2],
    symbol: Option<String>,
    symbol_kind: Option<SymbolKind>,
    pinned: bool,
    is_definition: bool,
    focus_line: u32,
    #[serde(deserialize_with = "score_text")]
    score: String,
    sources: Vec<(BackendId, u32)>,
}

fn score_text<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    f64::deserialize(deserializer).map(|score| format!("{score:.9}"))
}

impl From<&FusedHit> for Row {
    fn from(hit: &FusedHit) -> Self {
        Row {
            repo: hit.repo.clone(),
            path: hit.path.clone(),
            lines: [hit.lines.start, hit.lines.end],
            symbol: hit.symbol.clone(),
            symbol_kind: hit.symbol_kind,
            pinned: hit.pinned,
            is_definition: hit.is_definition,
            focus_line: hit.focus_line,
            score: format!("{:.9}", hit.score),
            sources: hit.sources.iter().map(|source| (source.backend, source.rank)).collect(),
        }
    }
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    let text = fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn classify_cases() -> Vec<ClassifyCase> {
    read(&golden_dir().join("classify.json"))
}

fn fusion_cases() -> Vec<FusionCase> {
    let mut paths: Vec<PathBuf> = fs::read_dir(golden_dir().join("fusion"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths.iter().map(|path| read(path)).collect()
}

#[test]
fn golden_files_are_well_formed() {
    assert!(classify_cases().len() >= 20);
    let cases = fusion_cases();
    assert!(cases.len() >= 7);
    for case in &cases {
        assert!(!case.description.is_empty() && !case.expected.is_empty(), "{}", case.name);
        // Hit::rank is the position in one backend's list for one repository: 0..n without gaps.
        let mut ranks: BTreeMap<(BackendId, &str), Vec<u32>> = BTreeMap::new();
        for hit in &case.hits {
            ranks.entry((hit.backend, hit.repo.as_str())).or_default().push(hit.rank);
        }
        for ((backend, repo), mut list) in ranks {
            list.sort_unstable();
            let expected: Vec<u32> = (0..list.len() as u32).collect();
            assert_eq!(list, expected, "{}: ranks of {backend} in {repo}", case.name);
        }
    }
}

#[test]
fn classify_matches_golden() {
    for case in classify_cases() {
        let got = classify(&case.query, case.forced);
        assert_eq!(
            (got.kind, got.text.as_str()),
            (case.kind, case.text.as_str()),
            "query {:?}, forced {:?}",
            case.query,
            case.forced
        );
    }
}

#[test]
fn fusion_matches_golden() {
    let config = FusionConfig::default();
    for case in fusion_cases() {
        let fused = fuse(&case.query, case.kind, case.hits.clone(), &case.spans, &config);
        let got: Vec<Row> = fused.iter().map(Row::from).collect();
        assert_eq!(got, case.expected, "case {}: {}", case.name, case.description);
    }
}
