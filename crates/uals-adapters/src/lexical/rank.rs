//! The ranker: scores raw matches, applies the down-rank rules, orders them.
//!
//! Score = tier x path factor.
//!
//! Tiers for a name (an `Identifier` query, or a `Literal` that looks like an identifier):
//! 1. 4.0, declaration: a line where `class|interface|object|enum|fun|def|fn|struct|trait|type|
//!    typealias|val|var|const|let|function` is followed by the name (case-exact).
//! 2. 3.0, the file name without extension equals the name.
//! 3. 2.0, the name occurs case-exact as a whole word (for a `Literal`: as a substring).
//! 4. 1.0, everything else (a smart-case match with other letter case, any `Regex` match).
//!
//! Tier for `Natural` queries: the number of distinct query words the line contains. The query
//! words are the runs of letters, digits and `_` of 4 or more characters, matched whole-word and
//! case-insensitively; a line with 3 of them scores 3.0, with 1 scores 1.0.
//!
//! The path factor is the factor of the first `downrank` rule whose pattern matches the path
//! relative to the repository root, or 1.0; it is not applied when the query contains `test`,
//! `mock` or `fake`. Ties go to the smaller path, then the smaller line.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;
use uals_core::config::Downrank;

use super::scan::RawMatch;

const DECLARATION_KEYWORDS: &str =
    "class|interface|object|enum|fun|def|fn|struct|trait|type|typealias|val|var|const|let|function";
const TIER_DECLARATION: f64 = 4.0;
const TIER_FILE_NAME: f64 = 3.0;
const TIER_EXACT: f64 = 2.0;
const TIER_REST: f64 = 1.0;
/// Shortest word of a `Natural` query that is searched; shorter ones match too much.
const MIN_WORD_CHARS: usize = 4;
/// A query containing one of these asks for test code, so test paths are not down-ranked.
const TEST_CODE_WORDS: [&str; 3] = ["test", "mock", "fake"];

/// A line where `name` is declared: the keyword, optional generics and receiver, then the name.
pub(super) fn declaration_regex(name: &str) -> Option<Regex> {
    let pattern = format!(
        r"(?:^|[^\w$])(?:{DECLARATION_KEYWORDS})\s+(?:<[^>]*>\s*)?(?:[\w$<>?,]+\.)?{}(?:[^\w$]|$)",
        regex::escape(name)
    );
    Regex::new(&pattern).ok()
}

/// The query words of a `Natural` query: distinct, lower case, 4 or more characters.
pub(super) fn natural_words(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut words = Vec::new();
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.chars().count() >= MIN_WORD_CHARS {
            let word = word.to_lowercase();
            if seen.insert(word.clone()) {
                words.push(word);
            }
        }
    }
    words
}

/// Whether `text` is one identifier: a letter, `_` or `$`, then letters, digits, `_` or `$`.
fn is_word_like(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// What the tiers are computed from.
pub(super) enum Profile {
    /// An identifier or literal: `declaration` finds its declaration lines, `exact` its case-exact
    /// occurrences, `file_stem` enables the file-name tier.
    Name { name: String, declaration: Option<Regex>, exact: Regex, file_stem: bool },
    /// A `Natural` query: the regex finds the query words of a line.
    Words(Regex),
    /// Every match has the lowest tier.
    Plain,
}

impl Profile {
    /// For an identifier: whole-word matches count as "exact".
    pub fn identifier(name: &str) -> Self {
        let exact = format!(r"(?:^|[^\w$]){}(?:[^\w$]|$)", regex::escape(name));
        Self::name(name, &exact, true)
    }

    /// For a literal: only identifier-like text has declarations and a file-name tier; anything
    /// else is "exact" when it occurs case-exact as a substring.
    pub fn literal(text: &str) -> Self {
        if is_word_like(text) { Self::identifier(text) } else { Self::name(text, &regex::escape(text), false) }
    }

    fn name(name: &str, exact_pattern: &str, word_like: bool) -> Self {
        match Regex::new(exact_pattern) {
            Ok(exact) => Profile::Name {
                name: name.to_owned(),
                declaration: if word_like { declaration_regex(name) } else { None },
                exact,
                file_stem: word_like,
            },
            Err(_) => Profile::Plain,
        }
    }

    /// For a `Natural` query: a line scores by how many distinct `words` it contains.
    pub fn words(words: &[String]) -> Self {
        let alternation: Vec<String> = words.iter().map(|word| regex::escape(word)).collect();
        match Regex::new(&format!(r"(?i)\b(?:{})\b", alternation.join("|"))) {
            Ok(regex) => Profile::Words(regex),
            Err(_) => Profile::Plain,
        }
    }

    /// The tier of one line and whether the line declares the name.
    fn tier(&self, path: &Path, line: &str) -> (f64, bool) {
        match self {
            Profile::Name { name, declaration, exact, file_stem } => {
                if declaration.as_ref().is_some_and(|declaration| declaration.is_match(line)) {
                    (TIER_DECLARATION, true)
                } else if *file_stem && path.file_stem().is_some_and(|stem| stem == name.as_str()) {
                    (TIER_FILE_NAME, false)
                } else if exact.is_match(line) {
                    (TIER_EXACT, false)
                } else {
                    (TIER_REST, false)
                }
            }
            Profile::Words(regex) => {
                let distinct: HashSet<String> =
                    regex.find_iter(line).map(|found| found.as_str().to_lowercase()).collect();
                (distinct.len().max(1) as f64, false)
            }
            Profile::Plain => (TIER_REST, false),
        }
    }

    /// The symbol name of definition hits.
    pub fn symbol(&self) -> Option<&str> {
        match self {
            Profile::Name { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// A match with its score.
pub(super) struct Ranked {
    pub path: PathBuf,
    pub line: u32,
    pub text: String,
    pub score: f64,
    pub definition: bool,
}

/// Scores matches and applies the path down-rank rules.
pub(super) struct Ranker {
    rules: Vec<(Regex, f64)>,
}

impl Ranker {
    /// Rules with a pattern that does not compile are dropped (`Config::validate` rejects them
    /// earlier; this only keeps a hand-built config from panicking).
    pub fn new(rules: &[Downrank]) -> Self {
        let rules = rules
            .iter()
            .filter_map(|rule| match Regex::new(&rule.pattern) {
                Ok(regex) => Some((regex, rule.factor)),
                Err(err) => {
                    tracing::warn!(pattern = %rule.pattern, %err, "lexical: ignoring a down-rank rule");
                    None
                }
            })
            .collect();
        Self { rules }
    }

    /// Whether `query` mentions tests, mocks or fakes.
    pub fn mentions_test_code(query: &str) -> bool {
        let query = query.to_lowercase();
        TEST_CODE_WORDS.iter().any(|word| query.contains(word))
    }

    /// The factor of the first rule that matches `relative`, 1.0 when none does.
    fn factor(&self, relative: &str) -> f64 {
        self.rules.iter().find(|(regex, _)| regex.is_match(relative)).map_or(1.0, |(_, factor)| *factor)
    }

    /// Scores and orders the matches, best first. `apply_downrank` switches the path factor on.
    pub fn rank(&self, root: &Path, profile: &Profile, apply_downrank: bool, matches: Vec<RawMatch>) -> Vec<Ranked> {
        let mut ranked: Vec<Ranked> = matches
            .into_iter()
            .map(|found| {
                let (tier, definition) = profile.tier(&found.path, &found.text);
                let factor = if apply_downrank { self.factor(&relative_slash(root, &found.path)) } else { 1.0 };
                Ranked { path: found.path, line: found.line, text: found.text, score: tier * factor, definition }
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)).then_with(|| a.line.cmp(&b.line))
        });
        ranked
    }
}

/// The path relative to `root` with `/` separators and no leading `/`.
fn relative_slash(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().map(|part| part.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}
