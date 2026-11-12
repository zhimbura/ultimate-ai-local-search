//! The path-glob filter every adapter applies to `SearchRequest::paths`.
//!
//! One meaning for all backends:
//! - a glob is gitignore-style and matches a path relative to the repository root, with `/`
//!   separators; `*.kt` matches in any folder, `src/**/*.kt` is anchored, `src/` matches a folder;
//! - a glob selects; a glob that starts with `!` deselects;
//! - a glob matches a file when it matches the file or one of its folders, so `app` covers
//!   everything below `app/`; the last glob that matches a file decides, and a file no glob
//!   matches is not selected;
//! - when every glob is a negation, selection starts from every file, as if `**` came first; an
//!   empty list selects every file.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use uals_core::backend::BackendError;

/// The compiled globs of one request, one matcher per glob in request order.
pub(crate) struct PathFilter {
    globs: Vec<Gitignore>,
}

impl PathFilter {
    /// Compiles `globs`; an invalid glob is `BackendError::Failed`.
    pub(crate) fn new(globs: &[String]) -> Result<Self, BackendError> {
        if globs.is_empty() {
            return Ok(Self { globs: Vec::new() });
        }
        let everything = globs.iter().all(|glob| glob.starts_with('!')).then_some("**");
        let compiled = everything
            .into_iter()
            .chain(globs.iter().map(String::as_str))
            .map(compile_glob)
            .collect::<Result<_, _>>()?;
        Ok(Self { globs: compiled })
    }

    /// Whether the filter has any glob, i.e. may drop paths.
    pub(crate) fn is_active(&self) -> bool {
        !self.globs.is_empty()
    }

    /// Whether the file at `relative` (a path relative to the repository root) is selected.
    pub(crate) fn allows(&self, relative: impl AsRef<Path>) -> bool {
        if self.globs.is_empty() {
            return true;
        }
        let relative = relative.as_ref();
        self.globs
            .iter()
            .rev()
            .map(|glob| glob.matched_path_or_any_parents(relative, false))
            .find(|decision| !decision.is_none())
            .is_some_and(|decision| decision.is_ignore())
    }
}

/// A matcher that holds the single glob `glob`; a negated glob answers "whitelist" when it matches.
fn compile_glob(glob: &str) -> Result<Gitignore, BackendError> {
    let mut builder = GitignoreBuilder::new("");
    builder
        .add_line(None, glob)
        .map_err(|error| BackendError::Failed(format!("invalid path glob `{glob}`: {error}")))?;
    builder.build().map_err(|error| BackendError::Failed(format!("invalid path glob `{glob}`: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (globs, path, selected)
    const CASES: &[(&[&str], &str, bool)] = &[
        (&[], "a/B.kt", true),
        (&["**/*.kt"], "a/B.kt", true),
        (&["**/*.kt"], "B.kt", true),
        (&["**/*.kt"], "a/B.ts", false),
        (&["*.kt"], "a/b/B.kt", true),
        (&["src/**/*.kt"], "src/x/Y.kt", true),
        (&["src/**/*.kt"], "other/src/x/Y.kt", false),
        (&["!*Test.kt"], "a/B.kt", true),
        (&["!*Test.kt"], "a/BTest.kt", false),
        (&["!*Test.kt", "!*.md"], "a/README.md", false),
        (&["*.kt", "!*Test.kt"], "a/B.kt", true),
        (&["*.kt", "!*Test.kt"], "a/BTest.kt", false),
        (&["*Test.kt", "*.kt"], "a/BTest.kt", true),
        (&["app", "!app/test"], "app/test/A.kt", false),
        (&["app", "!app/test"], "app/src/B.kt", true),
        (&["app", "!app/test"], "lib/B.kt", false),
        (&["src/"], "src/A.kt", true),
        (&["src/"], "x/src/A.kt", true),
        (&["src/"], "srcs/A.kt", false),
        (&["!app", "app/keep"], "app/keep/A.kt", true),
        (&["!app", "app/keep"], "app/other/A.kt", false),
        (&["!app", "app/keep"], "lib/A.kt", false),
    ];

    #[test]
    fn one_filter_one_meaning() {
        for (globs, path, selected) in CASES {
            let globs: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
            let filter = PathFilter::new(&globs).unwrap();
            assert_eq!(filter.allows(path), *selected, "globs {globs:?}, path {path}");
        }
    }

    #[test]
    fn an_invalid_glob_is_a_failed_backend() {
        let error = PathFilter::new(&["[z-a]".to_owned()]).err().expect("rejected");
        assert!(matches!(error, BackendError::Failed(message) if message.contains("invalid path glob")));
    }
}
