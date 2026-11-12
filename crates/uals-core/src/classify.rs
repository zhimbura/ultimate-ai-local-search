//! Query classification. The rules below and the cases in `tests/golden/classify.json` are the
//! contract.
//!
//! `classify(raw, None)` trims `raw`, then the first matching rule wins:
//! 1. **Regex**: starts and ends with `/`, with at least one character between. Text: the inside.
//! 2. **Literal**: wrapped in a matching pair of `"`, `'` or `` ` ``, with at least one character
//!    between. Text: the inside.
//! 3. **Identifier**: one token without whitespace, made of segments `[A-Za-z_$][A-Za-z0-9_$]*`
//!    joined by `.`, `::` or `#`, optionally ending in `()`, AND code-like: it contains `_`, `$`,
//!    a separator or `()`, or an uppercase letter. Examples: `HostSelector`, `markFailed`,
//!    `MAX_RETRIES`, `sample.core.Analytics`, `Foo::bar`, `retry()`. Text: the token.
//! 4. **Natural**: everything else, including one plain lowercase word (`retry`) and prose that
//!    mentions identifiers (`how does markFailed work`). Text: the trimmed query.
//!
//! `classify(raw, Some(kind))` keeps `kind` and only removes that kind's wrapper: the slashes for
//! Regex, the quotes for Literal (when present). Identifier and Natural text is only trimmed.
//!
//! The caller rejects blank queries before classifying.

use serde::{Deserialize, Serialize};

use crate::types::QueryKind;

/// A classified query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classified {
    /// The kind that was detected, or the one the caller forced.
    pub kind: QueryKind,
    /// The text sent to backends as `SearchRequest::query`.
    pub text: String,
}

/// Decides the kind of `raw` (or takes `forced`) and returns the text for backends.
pub fn classify(raw: &str, forced: Option<QueryKind>) -> Classified {
    let trimmed = raw.trim();
    match forced {
        Some(kind) => {
            let text = match kind {
                QueryKind::Regex => regex_inside(trimmed),
                QueryKind::Literal => literal_inside(trimmed),
                QueryKind::Identifier | QueryKind::Natural => None,
            };
            Classified { kind, text: text.unwrap_or(trimmed).to_string() }
        }
        None => detect(trimmed),
    }
}

/// Applies the rules in order; the first match wins.
fn detect(trimmed: &str) -> Classified {
    if let Some(inside) = regex_inside(trimmed) {
        return Classified { kind: QueryKind::Regex, text: inside.to_string() };
    }
    if let Some(inside) = literal_inside(trimmed) {
        return Classified { kind: QueryKind::Literal, text: inside.to_string() };
    }
    let kind = if is_identifier(trimmed) { QueryKind::Identifier } else { QueryKind::Natural };
    Classified { kind, text: trimmed.to_string() }
}

/// The text between a leading and a trailing `/`, when at least one character sits between them.
fn regex_inside(text: &str) -> Option<&str> {
    let inside = text.strip_prefix('/')?.strip_suffix('/')?;
    (!inside.is_empty()).then_some(inside)
}

/// The text between a matching pair of `"`, `'` or `` ` ``, when at least one character sits between.
fn literal_inside(text: &str) -> Option<&str> {
    let quote = text.chars().next().filter(|c| matches!(c, '"' | '\'' | '`'))?;
    let inside = text.strip_prefix(quote)?.strip_suffix(quote)?;
    (!inside.is_empty()).then_some(inside)
}

/// One token of `[A-Za-z_$][A-Za-z0-9_$]*` segments joined by `.`, `::` or `#`, optionally ending in
/// `()`, that looks like code: it has `_`, `$`, a separator, `()` or an uppercase letter.
fn is_identifier(token: &str) -> bool {
    let (body, has_call) = match token.strip_suffix("()") {
        Some(body) => (body, true),
        None => (token, false),
    };
    let mut rest = body;
    let mut has_separator = false;
    loop {
        let Some(after_segment) = skip_segment(rest) else { return false };
        rest = after_segment;
        if rest.is_empty() {
            break;
        }
        let Some(after_separator) = skip_separator(rest) else { return false };
        has_separator = true;
        rest = after_separator;
    }
    has_call || has_separator || body.chars().any(|c| c == '_' || c == '$' || c.is_ascii_uppercase())
}

/// The text after one leading `::`, `.` or `#`, or `None` when `text` does not start with one.
fn skip_separator(text: &str) -> Option<&str> {
    text.strip_prefix("::").or_else(|| text.strip_prefix(['.', '#']))
}

/// The text after one leading segment, or `None` when `text` does not start with a segment.
fn skip_segment(text: &str) -> Option<&str> {
    let is_start = |c: char| c.is_ascii_alphabetic() || c == '_' || c == '$';
    let is_part = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    let mut chars = text.char_indices();
    chars.next().filter(|&(_, c)| is_start(c))?;
    let end = chars.find(|&(_, c)| !is_part(c)).map_or(text.len(), |(index, _)| index);
    Some(&text[end..])
}

/// The simple name inside an identifier: the last segment after `.`, `::` or `#`, without a
/// trailing `()`. Backends use it for symbol lookups; fusion uses it to pin exact definitions.
///
/// `sample.core.Analytics` → `Analytics`, `Foo::bar()` → `bar`, `HostSelector` → `HostSelector`.
pub fn bare_name(identifier: &str) -> &str {
    let trimmed = identifier.trim();
    let trimmed = trimmed.strip_suffix("()").unwrap_or(trimmed);
    let start = trimmed.rfind(['.', ':', '#']).map_or(0, |index| index + 1);
    &trimmed[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_takes_the_last_segment() {
        assert_eq!(bare_name("HostSelector"), "HostSelector");
        assert_eq!(bare_name("sample.core.Analytics"), "Analytics");
        assert_eq!(bare_name("Foo::bar()"), "bar");
        assert_eq!(bare_name("HostSelector#pick"), "pick");
        assert_eq!(bare_name(" retry() "), "retry");
        assert_eq!(bare_name("R.string.app_name"), "app_name");
    }

    fn kind_of(raw: &str) -> QueryKind {
        classify(raw, None).kind
    }

    #[test]
    fn degenerate_wrappers_are_not_regex_or_literal() {
        assert_eq!(kind_of("//"), QueryKind::Natural);
        assert_eq!(kind_of("/"), QueryKind::Natural);
        assert_eq!(kind_of("\"\""), QueryKind::Natural);
        assert_eq!(kind_of("\""), QueryKind::Natural);
        assert_eq!(kind_of("``"), QueryKind::Natural);
    }

    #[test]
    fn identifier_shapes() {
        for query in ["Foo::bar()", "a.b", "a#b", "_x", "x_", "$", "a::b::C", "Retry", "a1B"] {
            assert_eq!(kind_of(query), QueryKind::Identifier, "{query}");
        }
        for query in
            ["a", "retry", "()", "a..b", "a.", ".a", "a::", "a:b", "a b", "a()b", "a().b", "1a", "a-b", "ü", "Über"]
        {
            assert_eq!(kind_of(query), QueryKind::Natural, "{query}");
        }
    }

    #[test]
    fn wrappers_win_over_identifiers_and_keep_inner_spaces() {
        assert_eq!(
            classify("\" HostSelector \"", None),
            Classified { kind: QueryKind::Literal, text: " HostSelector ".into() }
        );
        assert_eq!(classify("/Foo/", None), Classified { kind: QueryKind::Regex, text: "Foo".into() });
        assert_eq!(classify("  /a b/  ", None).text, "a b");
    }

    #[test]
    fn forced_kinds_strip_only_their_own_wrapper() {
        let forced = |raw: &str, kind| classify(raw, Some(kind)).text;
        assert_eq!(forced("\"a\"", QueryKind::Regex), "\"a\"");
        assert_eq!(forced("/a/", QueryKind::Literal), "/a/");
        assert_eq!(forced("/a/", QueryKind::Natural), "/a/");
        assert_eq!(forced(" \"a\" ", QueryKind::Identifier), "\"a\"");
        assert_eq!(forced("//", QueryKind::Regex), "//");
        assert_eq!(forced("\"\"", QueryKind::Literal), "\"\"");
        assert_eq!(forced("'a\"", QueryKind::Literal), "'a\"");
    }

    #[test]
    fn empty_input_is_natural() {
        assert_eq!(classify("", None), Classified { kind: QueryKind::Natural, text: String::new() });
    }
}
