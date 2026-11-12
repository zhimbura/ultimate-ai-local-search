//! The path-glob filter shared by the adapters whose backends report repository-relative paths
//! (`ast-index`, `claude-context`).

use std::path::Path;

use ignore::overrides::{Override, OverrideBuilder};
use uals_core::backend::BackendError;

/// `SearchRequest::paths` as a whitelist: globs relative to the repository root. An empty list
/// allows every path.
pub(crate) struct PathFilter {
    overrides: Option<Override>,
}

impl PathFilter {
    /// Compiles `globs` relative to `root`; an invalid glob is `BackendError::Failed`.
    pub(crate) fn new(root: &Path, globs: &[String]) -> Result<Self, BackendError> {
        if globs.is_empty() {
            return Ok(Self { overrides: None });
        }
        let mut builder = OverrideBuilder::new(root);
        for glob in globs {
            builder.add(glob).map_err(|error| BackendError::Failed(format!("invalid path glob `{glob}`: {error}")))?;
        }
        let overrides =
            builder.build().map_err(|error| BackendError::Failed(format!("invalid path globs: {error}")))?;
        Ok(Self { overrides: Some(overrides) })
    }

    /// Whether the filter has any glob, i.e. may drop paths.
    pub(crate) fn is_active(&self) -> bool {
        self.overrides.is_some()
    }

    /// True when `relative` (a file path relative to the root) or one of its folders matches a
    /// glob, so `app/` selects everything below it.
    pub(crate) fn allows(&self, relative: impl AsRef<Path>) -> bool {
        let Some(overrides) = &self.overrides else {
            return true;
        };
        let path = relative.as_ref();
        if overrides.matched(path, false).is_whitelist() {
            return true;
        }
        path.ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .any(|ancestor| overrides.matched(ancestor, true).is_whitelist())
    }
}
