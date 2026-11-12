//! Gold set files (JSONL): queries with expected locations, and symbols with expected usages.
//!
//! A line that cannot be parsed or fails validation is reported as `<file>:<line>: <reason>`.

use std::collections::HashSet;

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// What the query tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum QueryType {
    Identifier,
    Natural,
    CrossRepo,
    Config,
}

impl QueryType {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryType::Identifier => "identifier",
            QueryType::Natural => "natural",
            QueryType::CrossRepo => "cross_repo",
            QueryType::Config => "config",
        }
    }
}

/// The half of the gold set: A is for tuning, B is held out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Half {
    A,
    B,
}

impl Half {
    pub fn as_str(self) -> &'static str {
        match self {
            Half::A => "A",
            Half::B => "B",
        }
    }
}

/// How well an expected location answers the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Primary,
    Secondary,
}

/// A place in the code that answers a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    pub repo: String,
    /// Relative to the repository root, with `/` separators.
    pub path: String,
    /// `[start, end]`, 1-based and inclusive.
    pub lines: [u32; 2],
    #[serde(default)]
    pub role: Role,
}

/// One line of the queries file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldQuery {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: QueryType,
    pub half: Half,
    pub query: String,
    pub expected: Vec<Expected>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Where a symbol is defined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub repo: String,
    pub path: String,
    pub line: u32,
}

/// Expected usages of a symbol in one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedUsage {
    pub repo: String,
    pub path: String,
    /// The usage lines (1-based), one number per usage.
    pub lines: Vec<u32>,
}

/// One line of the usages file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldUsages {
    pub id: String,
    pub symbol: String,
    pub definition: Definition,
    pub expected_usages: Vec<ExpectedUsage>,
    #[serde(default)]
    pub method: Option<String>,
}

/// A record of a JSONL gold file.
pub trait Record {
    fn id(&self) -> &str;
    /// Checks what serde cannot; the error is a reason without a position.
    fn validate(&self) -> std::result::Result<(), String>;
}

impl Record for GoldQuery {
    fn id(&self) -> &str {
        &self.id
    }

    fn validate(&self) -> std::result::Result<(), String> {
        if self.id.is_empty() || self.query.is_empty() {
            return Err("`id` and `query` must not be empty".into());
        }
        if !self.expected.iter().any(|e| e.role == Role::Primary) {
            return Err("`expected` needs at least one entry with role \"primary\"".into());
        }
        for e in &self.expected {
            let [start, end] = e.lines;
            if start == 0 || start > end {
                return Err(format!("bad line range [{start}, {end}] in {}: need 1 <= start <= end", e.path));
            }
        }
        Ok(())
    }
}

impl Record for GoldUsages {
    fn id(&self) -> &str {
        &self.id
    }

    fn validate(&self) -> std::result::Result<(), String> {
        if self.id.is_empty() || self.symbol.is_empty() {
            return Err("`id` and `symbol` must not be empty".into());
        }
        if self.expected_usages.iter().map(|u| u.lines.len()).sum::<usize>() == 0 {
            return Err("`expected_usages` needs at least one line".into());
        }
        if self.expected_usages.iter().flat_map(|u| &u.lines).any(|&l| l == 0) {
            return Err("line numbers start at 1".into());
        }
        Ok(())
    }
}

/// Parses a JSONL text. `name` labels the errors (use the file name). Blank lines are skipped.
pub fn parse_jsonl<T>(text: &str, name: &str) -> Result<Vec<T>>
where
    T: Record + for<'de> Deserialize<'de>,
{
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let record: T = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(error) => bail!("{name}:{number}: {error}"),
        };
        if let Err(reason) = record.validate() {
            bail!("{name}:{number}: {reason}");
        }
        if !seen.insert(record.id().to_string()) {
            bail!("{name}:{number}: duplicate id \"{}\"", record.id());
        }
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"id":"q1","type":"identifier","half":"A","query":"Foo","expected":[{"repo":"r","path":"a.kt","lines":[1,5],"role":"primary"}],"notes":"n"}"#;

    #[test]
    fn parses_a_query_and_defaults_the_role() {
        let line = r#"{"id":"q1","type":"cross_repo","half":"B","query":"x","expected":[{"repo":"r","path":"a","lines":[3,3]}]}"#;
        let parsed: Vec<GoldQuery> = parse_jsonl(line, "g.jsonl").unwrap();
        assert_eq!(parsed[0].kind, QueryType::CrossRepo);
        assert_eq!(parsed[0].half, Half::B);
        assert_eq!(parsed[0].expected[0].role, Role::Primary);
        assert_eq!(parsed[0].notes, None);
    }

    #[test]
    fn error_names_the_line_of_bad_json() {
        let text = format!("{GOOD}\n\n{{\"id\":\"q2\",\"type\":\"bogus\"}}\n");
        let error = parse_jsonl::<GoldQuery>(&text, "gold.jsonl").unwrap_err().to_string();
        assert!(error.starts_with("gold.jsonl:3: "), "{error}");
        assert!(error.contains("bogus") || error.contains("unknown variant"), "{error}");
    }

    #[test]
    fn error_names_the_line_of_a_bad_range_and_missing_primary() {
        let range = GOOD.replace("[1,5]", "[9,2]");
        let error = parse_jsonl::<GoldQuery>(&format!("{GOOD}\n{}", range.replace("q1", "q2")), "g").unwrap_err();
        assert!(error.to_string().starts_with("g:2: bad line range [9, 2]"), "{error}");

        let secondary = GOOD.replace("primary", "secondary");
        let error = parse_jsonl::<GoldQuery>(&secondary, "g").unwrap_err();
        assert!(error.to_string().starts_with("g:1: "), "{error}");
        assert!(error.to_string().contains("primary"), "{error}");
    }

    #[test]
    fn duplicate_ids_are_rejected_with_the_second_line() {
        let error = parse_jsonl::<GoldQuery>(&format!("{GOOD}\n{GOOD}"), "g").unwrap_err();
        assert_eq!(error.to_string(), "g:2: duplicate id \"q1\"");
    }

    #[test]
    fn parses_usages_and_validates_them() {
        let good = r#"{"id":"u1","symbol":"S","definition":{"repo":"r","path":"a","line":2},"expected_usages":[{"repo":"r","path":"b","lines":[4,9]}],"method":"grep"}"#;
        let parsed: Vec<GoldUsages> = parse_jsonl(good, "u").unwrap();
        assert_eq!(parsed[0].expected_usages[0].lines, vec![4, 9]);

        let empty = good.replace("[4,9]", "[]");
        let error = parse_jsonl::<GoldUsages>(&empty, "u").unwrap_err();
        assert!(error.to_string().starts_with("u:1: "), "{error}");
        let error = parse_jsonl::<GoldUsages>("{\"id\":\"u1\"}", "u").unwrap_err();
        assert!(error.to_string().starts_with("u:1: missing field"), "{error}");
    }
}
