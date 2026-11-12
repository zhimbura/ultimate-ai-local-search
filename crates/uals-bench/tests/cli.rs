//! End to end: the binary on the committed fixtures.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(name)
}

fn bench(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_uals-bench")).args(args).output().expect("run uals-bench")
}

fn ok(args: &[&str]) -> String {
    let out = bench(args);
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// Runs the fixture gold set against one mock and returns the report JSON.
fn run_fixture(dir: &Path, name: &str, mock: &str, extra: &[&str]) -> (PathBuf, Value) {
    let out = dir.join(format!("{name}.json"));
    let engine = format!("mock:{}", fixture(mock).display());
    let gold = fixture("gold.jsonl");
    let usages = fixture("usages.jsonl");
    let mut args = vec![
        "run",
        "--gold",
        gold.to_str().unwrap(),
        "--usages",
        usages.to_str().unwrap(),
        "--engine",
        &engine,
        "--name",
        name,
        "--out",
        out.to_str().unwrap(),
        "--warm-repeats",
        "2",
    ];
    args.extend_from_slice(extra);
    ok(&args);
    let report = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    (out, report)
}

fn close(value: &Value, expected: f64) {
    let got = value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"));
    assert!((got - expected).abs() < 1e-9, "{got} != {expected}");
}

#[test]
fn run_reports_hand_computed_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let (_, better) = run_fixture(dir.path(), "better", "mock-better.json", &[]);
    let overall = &better["summary"]["overall"];
    assert_eq!(overall["n"], 12);
    close(&overall["primary"]["hit_at_1"], 11.0 / 12.0);
    close(&overall["primary"]["hit_at_5"], 1.0);
    close(&overall["primary"]["mrr"], (11.0 + 0.5) / 12.0);
    assert_eq!(better["summary"]["latency"]["warm_samples"], 24);
    assert_eq!(better["usages"].as_array().unwrap().len(), 4);
    close(&better["summary"]["usages"]["line_recall"], 1.0);

    let (_, worse) = run_fixture(dir.path(), "worse", "mock-worse.json", &[]);
    let overall = &worse["summary"]["overall"];
    // ranks: 3, 2, -, 1, -, 2, 2, 1, (failed), 2, 1, 3
    let expected_mrr = (1.0 / 3.0 + 0.5 + 0.0 + 1.0 + 0.0 + 0.5 + 0.5 + 1.0 + 0.0 + 0.5 + 1.0 + 1.0 / 3.0) / 12.0;
    close(&overall["primary"]["mrr"], expected_mrr);
    assert_eq!(overall["failed"], 1);
    close(&overall["primary"]["hit_at_1"], 3.0 / 12.0);
    // secondary entries rescue q002 (rank 1), q003 (1) and q006 (1) and q005 stays a miss
    assert!(overall["with_secondary"]["mrr"].as_f64().unwrap() > expected_mrr);
    let q005 = worse["queries"].as_array().unwrap().iter().find(|q| q["id"] == "q005").unwrap();
    assert_eq!(q005["rank_primary"], Value::Null, "right file, lines do not overlap");
    assert_eq!(q005["rank_any"], Value::Null);
    let q007 = worse["queries"].as_array().unwrap().iter().find(|q| q["id"] == "q007").unwrap();
    assert_eq!(q007["rank_primary"], 2, "the same relative path in another repository is wrong");
    // u04 has no canned answer: the call failed, so it is marked and kept out of the means
    let u04 = worse["usages"].as_array().unwrap().iter().find(|u| u["id"] == "u04").unwrap();
    assert_eq!(u04["failed"], true);
    assert_eq!(worse["summary"]["usages"]["failed"], 1);
    assert_eq!(worse["summary"]["usages"]["symbols"], 3);
    // a report never carries paths or query text
    let text = worse.to_string();
    assert!(!text.contains(".kt") && !text.contains("HostSelector") && !text.contains("/ws/"));
}

#[test]
fn filters_by_half_and_type() {
    let dir = tempfile::tempdir().unwrap();
    let (_, half_a) = run_fixture(dir.path(), "a", "mock-better.json", &["--half", "A"]);
    assert_eq!(half_a["summary"]["overall"]["n"], 7);
    assert!(half_a["summary"]["by_half"].get("B").is_none());
    let (_, types) = run_fixture(dir.path(), "t", "mock-better.json", &["--type", "natural", "--type", "config"]);
    assert_eq!(types["summary"]["overall"]["n"], 6);
    assert!(types["summary"]["by_type"].get("identifier").is_none());
    let (_, both) = run_fixture(dir.path(), "b", "mock-better.json", &["--half", "B", "--type", "config"]);
    assert_eq!(both["summary"]["overall"]["n"], 1);
    let (_, all) = run_fixture(dir.path(), "all", "mock-better.json", &["--half", "all"]);
    assert_eq!(all["summary"]["overall"]["n"], 12);
}

#[test]
fn compare_is_deterministic_and_signed() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = run_fixture(dir.path(), "better", "mock-better.json", &[]);
    let (b, _) = run_fixture(dir.path(), "worse", "mock-worse.json", &[]);
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());

    let first = ok(&["compare", a, b, "--json"]);
    assert_eq!(first, ok(&["compare", a, b, "--json"]), "fixed seed: identical output");
    let result: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(result["paired"], 12);
    assert_eq!(result["resamples"], 10_000);
    let overall = &result["groups"][0];
    assert_eq!(overall["group"], "overall");
    close(&overall["mean_a"], 11.5 / 12.0);
    assert!(overall["diff"].as_f64().unwrap() < -0.4);
    assert!(overall["ci_high"].as_f64().unwrap() < 0.0, "the drop is significant");
    assert_eq!(overall["significant"], true);

    let swapped: Value = serde_json::from_str(&ok(&["compare", b, a, "--json"])).unwrap();
    close(&swapped["groups"][0]["diff"], -overall["diff"].as_f64().unwrap());

    let text = ok(&["compare", a, b, "--metric", "hit1"]);
    assert!(text.contains("`hit1`") && text.contains("| overall | 12 |"), "{text}");
    let types: Vec<&str> = result["groups"].as_array().unwrap().iter().map(|g| g["group"].as_str().unwrap()).collect();
    assert_eq!(types, ["overall", "config", "cross_repo", "identifier", "natural"]);
}

#[test]
fn compare_pairs_by_id_when_runs_cover_different_queries() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = run_fixture(dir.path(), "better", "mock-better.json", &[]);
    let (b, _) = run_fixture(dir.path(), "half", "mock-worse.json", &["--half", "A"]);
    let result: Value =
        serde_json::from_str(&ok(&["compare", a.to_str().unwrap(), b.to_str().unwrap(), "--json"])).unwrap();
    assert_eq!(result["paired"], 7);
    assert_eq!(result["only_in_a"], 5);
    assert_eq!(result["only_in_b"], 0);
}

#[test]
fn markdown_report_has_tables_and_no_code() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = run_fixture(dir.path(), "better", "mock-better.json", &[]);
    let (b, _) = run_fixture(dir.path(), "worse", "mock-worse.json", &[]);
    let md = ok(&["report", a.to_str().unwrap(), b.to_str().unwrap(), "--markdown"]);
    for needle in [
        "## Overall",
        "## Type: natural",
        "## Half: B",
        "## Usages",
        "| q005 | natural | A | 2/2 | -/- |",
        "| q009 | cross_repo | B | 1/1 | err |",
        "| u04 | err | err | err | err | - | - |",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in:\n{md}");
    }
    assert!(!md.contains("class ") && !md.contains("fun "), "no code in reports");
    let lines: Vec<&str> = md.lines().collect();
    for (header, separator) in lines.iter().zip(lines.iter().skip(1)).filter(|(_, next)| next.starts_with("|---")) {
        let columns = |line: &str| line.matches('|').count();
        assert_eq!(columns(header), columns(separator), "GFM needs equal columns:\n{header}\n{separator}");
    }
    let json: Value = serde_json::from_str(&ok(&["report", a.to_str().unwrap(), "--json"])).unwrap();
    assert_eq!(json[0]["name"], "better");
}

#[test]
fn gold_parse_errors_name_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let gold = dir.path().join("gold.jsonl");
    let good = std::fs::read_to_string(fixture("gold.jsonl")).unwrap();
    let first = good.lines().next().unwrap();
    std::fs::write(&gold, format!("{first}\n{{\"id\": \"broken\"\n")).unwrap();
    let engine = format!("mock:{}", fixture("mock-better.json").display());
    let out = bench(&["run", "--gold", gold.to_str().unwrap(), "--engine", &engine, "--name", "x"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("gold.jsonl:2:"), "{stderr}");

    let usages = dir.path().join("usages.jsonl");
    std::fs::write(&usages, "\n\n{\"id\":\"u1\",\"symbol\":\"S\"}\n").unwrap();
    let out = bench(&[
        "run",
        "--gold",
        fixture("gold.jsonl").to_str().unwrap(),
        "--usages",
        usages.to_str().unwrap(),
        "--engine",
        &engine,
        "--name",
        "x",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usages.jsonl:3: missing field"));
}

#[test]
fn unknown_engine_and_bad_report_fail_cleanly() {
    let gold = fixture("gold.jsonl");
    let out = bench(&["run", "--gold", gold.to_str().unwrap(), "--engine", "semble", "--name", "x"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown engine"));

    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.json");
    std::fs::write(&junk, "{}").unwrap();
    let out = bench(&["report", junk.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a uals-bench report"));
}
