//! Text and JSON for `status` and `refresh`; `uals_core::render` covers `search` and `usages`.
//!
//! **Status, text:**
//! ```text
//! status of /ws · 2 repositories
//! sample-android · /ws/sample-android
//!   ast-index: indexed, updated 3 h ago
//!   semble: not indexed (not checked: Semble builds the index on the first search …)
//! sample-web · /ws/sample-web
//!   ast-index: not indexed (no index; run `uals refresh`)
//! ```
//! One line per backend: `indexed` or `not indexed`, then the age of the last update when known,
//! `stale` with its reason, and the backend's own detail in parentheses.
//!
//! **Refresh, text:** one line per repository with the result of every backend, as
//! `backend ok 120 ms`, `backend unavailable (reason) 3 ms` or `backend failed (reason) 9 ms`.

use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;
use uals_core::backend::BackendError;
use uals_core::render::Format;
use uals_core::types::{IndexState, Repo};

use crate::orchestrator::RefreshResult;

const MS_PER_MINUTE: u64 = 60_000;
const MS_PER_HOUR: u64 = 60 * MS_PER_MINUTE;
const MS_PER_DAY: u64 = 24 * MS_PER_HOUR;

/// `status` as JSON.
#[derive(Serialize)]
struct StatusJson<'a> {
    workspace: &'a Path,
    repos: Vec<RepoStatusJson<'a>>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    language_servers: &'a [String],
}

#[derive(Serialize)]
struct RepoStatusJson<'a> {
    name: &'a str,
    root: &'a Path,
    backends: Vec<&'a IndexState>,
}

/// Renders the index states of `repos` in the workspace `root`. `now_ms` is the current Unix time
/// in milliseconds, for the age of each index.
/// `servers` has one line per running language server.
pub fn render_status(
    root: &Path,
    repos: &[Repo],
    states: &[IndexState],
    servers: &[String],
    format: Format,
    now_ms: u64,
) -> String {
    let states_of = |repo: &Repo| states.iter().filter(|state| state.repo == repo.name).collect::<Vec<_>>();
    if format == Format::Json {
        let repos = repos
            .iter()
            .map(|repo| RepoStatusJson { name: &repo.name, root: &repo.root, backends: states_of(repo) })
            .collect();
        return to_json(&StatusJson { workspace: root, repos, language_servers: servers });
    }
    let mut out = format!("status of {} · {}", root.display(), count(repos.len(), "repository", "repositories"));
    for repo in repos {
        let _ = write!(out, "\n{} · {}", repo.name, repo.root.display());
        for state in states_of(repo) {
            let _ = write!(out, "\n  {}", state_line(state, now_ms));
        }
    }
    if !servers.is_empty() {
        out.push_str("\nlanguage servers:");
        for server in servers {
            let _ = write!(out, "\n  {server}");
        }
    }
    out
}

/// `backend: indexed, updated 3 h ago, stale (reason) (detail)` for one index state.
fn state_line(state: &IndexState, now_ms: u64) -> String {
    let mut line = format!("{}: {}", state.backend, if state.indexed { "indexed" } else { "not indexed" });
    if let Some(updated) = state.updated_at_ms {
        let _ = write!(line, ", updated {} ago", age(now_ms.saturating_sub(updated)));
    }
    if let Some(reason) = &state.stale {
        let _ = write!(line, ", stale ({reason})");
    }
    if let Some(detail) = state.detail.as_deref().filter(|detail| !detail.is_empty()) {
        let _ = write!(line, " ({detail})");
    }
    line
}

/// A duration in the largest whole unit that fits: `45 s`, `12 min`, `3 h`, `2 d`.
fn age(ms: u64) -> String {
    match ms {
        _ if ms < MS_PER_MINUTE => format!("{} s", ms / 1000),
        _ if ms < MS_PER_HOUR => format!("{} min", ms / MS_PER_MINUTE),
        _ if ms < 2 * MS_PER_DAY => format!("{} h", ms / MS_PER_HOUR),
        _ => format!("{} d", ms / MS_PER_DAY),
    }
}

/// `refresh` as JSON: one entry per backend and repository.
#[derive(Serialize)]
struct RefreshJson<'a> {
    backend: String,
    repo: &'a str,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    elapsed_ms: u64,
}

/// Renders refresh results, one line per repository in the order of `repos`.
pub fn render_refresh(repos: &[Repo], results: &[RefreshResult], format: Format) -> String {
    if format == Format::Json {
        let entries: Vec<RefreshJson> = results
            .iter()
            .map(|result| {
                let (state, reason) = outcome(&result.result);
                RefreshJson {
                    backend: result.backend.to_string(),
                    repo: &result.repo,
                    state,
                    reason,
                    elapsed_ms: result.elapsed_ms,
                }
            })
            .collect();
        return to_json(&entries);
    }
    let lines: Vec<String> = repos
        .iter()
        .map(|repo| {
            let entries: Vec<String> = results
                .iter()
                .filter(|result| result.repo == repo.name)
                .map(|result| {
                    let (state, reason) = outcome(&result.result);
                    let reason = reason.map(|reason| format!(" ({reason})")).unwrap_or_default();
                    format!("{} {state}{reason} {} ms", result.backend, result.elapsed_ms)
                })
                .collect();
            format!("{}: {}", repo.name, entries.join(", "))
        })
        .collect();
    lines.join("\n")
}

/// True when any refresh failed; an unavailable backend (not installed, not running) is not a
/// failure.
pub fn any_refresh_failed(results: &[RefreshResult]) -> bool {
    results.iter().any(|result| matches!(result.result, Err(BackendError::Failed(_))))
}

/// The state word and reason of one refresh result.
fn outcome(result: &Result<(), BackendError>) -> (&'static str, Option<&str>) {
    match result {
        Ok(()) => ("ok", None),
        Err(BackendError::Unavailable(reason)) => ("unavailable", Some(reason)),
        Err(BackendError::Failed(reason)) => ("failed", Some(reason)),
    }
}

/// The current Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// `1 repository`, `3 repositories`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Compact JSON of `value`; an encoding failure becomes a JSON error object.
fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| format!("{{\"error\":\"cannot encode the response: {err}\"}}"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use uals_core::types::BackendId;

    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn repo(name: &str) -> Repo {
        Repo { name: name.into(), root: PathBuf::from(format!("/ws/{name}")) }
    }

    fn state(backend: BackendId, repo: &str, indexed: bool) -> IndexState {
        IndexState { backend, repo: repo.into(), indexed, updated_at_ms: None, stale: None, detail: None }
    }

    #[test]
    fn status_text_lists_every_backend_of_every_repository() {
        let repos = [repo("android"), repo("web")];
        let states = [
            IndexState {
                updated_at_ms: Some(NOW - 3 * MS_PER_HOUR - 5),
                stale: Some("2 files changed".into()),
                ..state(BackendId::AstIndex, "android", true)
            },
            IndexState { detail: Some("not checked".into()), ..state(BackendId::Semble, "android", false) },
            IndexState { detail: Some(String::new()), ..state(BackendId::AstIndex, "web", false) },
        ];

        let text = render_status(Path::new("/ws"), &repos, &states, &[], Format::Concise, NOW);

        assert_eq!(
            text,
            "status of /ws · 2 repositories\n\
             android · /ws/android\n  \
             ast-index: indexed, updated 3 h ago, stale (2 files changed)\n  \
             semble: not indexed (not checked)\n\
             web · /ws/web\n  \
             ast-index: not indexed"
        );
    }

    #[test]
    fn status_json_groups_the_states_by_repository() {
        let states = [state(BackendId::Lexical, "web", true), state(BackendId::AstIndex, "android", false)];

        let json = render_status(Path::new("/ws"), &[repo("android")], &states, &[], Format::Json, NOW);

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "workspace": "/ws",
                "repos": [{"name": "android", "root": "/ws/android",
                           "backends": [{"backend": "ast-index", "repo": "android", "indexed": false}]}]
            })
        );
    }

    #[test]
    fn ages_use_the_largest_whole_unit() {
        for (ms, expected) in [
            (0, "0 s"),
            (59_999, "59 s"),
            (MS_PER_MINUTE, "1 min"),
            (MS_PER_HOUR - 1, "59 min"),
            (MS_PER_HOUR, "1 h"),
            (2 * MS_PER_DAY - 1, "47 h"),
            (2 * MS_PER_DAY, "2 d"),
        ] {
            assert_eq!(age(ms), expected, "{ms} ms");
        }
    }

    fn refreshed(backend: BackendId, repo: &str, result: Result<(), BackendError>) -> RefreshResult {
        RefreshResult { backend, repo: repo.into(), result, elapsed_ms: 7 }
    }

    #[test]
    fn refresh_text_has_one_line_per_repository_and_only_failures_fail() {
        let results = [
            refreshed(BackendId::AstIndex, "android", Ok(())),
            refreshed(BackendId::Semble, "android", Err(BackendError::Unavailable("not installed".into()))),
            refreshed(BackendId::AstIndex, "web", Err(BackendError::Failed("disk full".into()))),
        ];

        let text = render_refresh(&[repo("android"), repo("web")], &results, Format::Concise);

        assert_eq!(
            text,
            "android: ast-index ok 7 ms, semble unavailable (not installed) 7 ms\nweb: ast-index failed (disk full) 7 ms"
        );
        assert!(any_refresh_failed(&results));
        assert!(!any_refresh_failed(&results[..2]), "an unavailable backend is not a failure");
    }

    #[test]
    fn refresh_json_has_one_entry_per_result() {
        let results = [refreshed(BackendId::Semble, "web", Err(BackendError::Unavailable("down".into())))];

        let json = render_refresh(&[repo("web")], &results, Format::Json);

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value,
            serde_json::json!([{"backend": "semble", "repo": "web", "state": "unavailable", "reason": "down", "elapsed_ms": 7}])
        );
    }
}
