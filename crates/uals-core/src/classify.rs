//! Query classification. `classify` is implemented by track T6; the rules below and the cases in
//! `tests/golden/classify.json` are the contract. `bare_name` is part of the frozen core.
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
    pub kind: QueryKind,
    /// The text sent to backends as `SearchRequest::query`.
    pub text: String,
}

/// Decides the kind of `raw` (or takes `forced`) and returns the text for backends.
pub fn classify(raw: &str, forced: Option<QueryKind>) -> Classified {
    let _ = (raw, forced);
    todo!("track T6: implement the rules in the module docs")
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
}
