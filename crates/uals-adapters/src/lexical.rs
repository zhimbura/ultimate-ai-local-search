//! In-process lexical search (track T3) with the ripgrep crates `ignore`, `grep-searcher` and
//! `grep-regex`, plus its ranker. Findings, the recommended walker config and the list of
//! regression tests: `docs/spikes/s3-lexical.md`.
//!
//! Matching by query kind (the query text is `SearchRequest::query`):
//! - `Identifier`: `bare_name(query)`, fixed string, whole word, smart case.
//! - `Literal`: the text, fixed string, smart case.
//! - `Regex`: the text as a regex, smart case; an invalid regex is `BackendError::Failed`.
//! - `Natural`: the words of 4 or more characters as a case-insensitive whole-word alternation;
//!   lines rank by how many distinct words they contain (see `rank`).
//!
//! The ranking rules are documented in `rank`. Caps (`LexicalConfig`): `max_matches_per_file`
//! while searching, `max_candidates` per repository (the scan stops when a match beyond it
//! exists), `max_hits_per_file` after ranking, then `SearchRequest::fetch_limit`.
//!
//! The scan stops by itself at 90% of the deadline. The answer is `partial` when the deadline or
//! the candidate cap cut it; the per-file and fetch caps do not set it.

mod rank;
mod scan;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use uals_core::backend::{BackendError, SearchBackend, UsageSource};
use uals_core::classify::bare_name;
use uals_core::config::LexicalConfig;
use uals_core::types::{
    BackendAnswer, BackendId, Hit, IndexState, LineRange, QueryKind, Repo, SearchRequest, Usage, UsageAnswer,
};

use rank::{Profile, Ranker, declaration_regex, natural_words};
use scan::{ScanOutput, ScanPlan, scan};

/// The scan uses this share of the orchestrator's deadline; the rest is for ranking and the
/// hand-over of the answer.
const DEADLINE_SHARE: u32 = 9;
const DEADLINE_DIVISOR: u32 = 10;
/// Used when `now + deadline` does not fit into an `Instant`.
const FAR_FUTURE: Duration = Duration::from_secs(365 * 24 * 3600);

/// Text search over the files of a repository, honoring its ignore files.
pub struct LexicalBackend {
    config: LexicalConfig,
    deadline: Duration,
    ranker: Arc<Ranker>,
}

/// A query turned into a matcher and the data the ranker needs.
struct Query {
    matcher: RegexMatcher,
    profile: Profile,
}

impl LexicalBackend {
    /// `deadline` is the orchestrator's deadline for this backend; the search stops a little
    /// earlier on its own and returns a partial answer, because blocking threads cannot be
    /// cancelled by dropping the future.
    pub fn new(config: LexicalConfig, deadline: Duration) -> Self {
        let ranker = Arc::new(Ranker::new(&config.downrank));
        Self { config, deadline, ranker }
    }

    fn stop_at(&self, start: Instant) -> Instant {
        let scan_time = self.deadline / DEADLINE_DIVISOR * DEADLINE_SHARE;
        start.checked_add(scan_time).unwrap_or_else(|| start + FAR_FUTURE)
    }

    fn plan(&self, repo: &Repo, matcher: RegexMatcher, start: Instant) -> ScanPlan {
        ScanPlan {
            root: repo.root.clone(),
            only_paths: Vec::new(),
            matcher,
            skip_line: None,
            per_file_cap: self.config.max_matches_per_file,
            global_cap: self.config.max_candidates,
            max_filesize: self.config.max_file_size_bytes,
            max_line_chars: self.config.max_line_chars,
            stop_at: self.stop_at(start),
        }
    }
}

fn check_root(repo: &Repo) -> Result<(), BackendError> {
    if repo.root.is_dir() {
        Ok(())
    } else {
        Err(BackendError::Unavailable(format!("repository root {} is not a directory", repo.root.display())))
    }
}

fn matcher_error(err: impl std::fmt::Display) -> BackendError {
    BackendError::Failed(format!("invalid regex: {err}"))
}

/// `None` means the query has nothing to look for (no name, no word of 4 characters).
fn build_query(request: &SearchRequest) -> Result<Option<Query>, BackendError> {
    let text = request.query.as_str();
    let fixed = |pattern: &str, word: bool| {
        RegexMatcherBuilder::new().case_smart(true).fixed_strings(true).word(word).build(pattern).map_err(matcher_error)
    };
    let query = match request.kind {
        QueryKind::Identifier => {
            let name = bare_name(text);
            if name.is_empty() {
                return Ok(None);
            }
            Query { matcher: fixed(name, true)?, profile: Profile::identifier(name) }
        }
        QueryKind::Literal => {
            if text.is_empty() {
                return Ok(None);
            }
            Query { matcher: fixed(text, false)?, profile: Profile::literal(text) }
        }
        QueryKind::Regex => Query {
            matcher: RegexMatcherBuilder::new().case_smart(true).build(text).map_err(matcher_error)?,
            profile: Profile::Plain,
        },
        QueryKind::Natural => {
            let words = natural_words(text);
            if words.is_empty() {
                return Ok(None);
            }
            let alternation: Vec<String> = words.iter().map(|word| regex::escape(word)).collect();
            let matcher = RegexMatcherBuilder::new()
                .case_insensitive(true)
                .word(true)
                .build(&alternation.join("|"))
                .map_err(matcher_error)?;
            Query { matcher, profile: Profile::words(&words) }
        }
    };
    Ok(Some(query))
}

fn partial_reason(output: &ScanOutput, cap: usize) -> Option<String> {
    let mut reasons = Vec::new();
    if output.timed_out {
        reasons.push("deadline reached before every file was searched".to_owned());
    }
    if output.capped {
        reasons.push(format!("candidate cap of {cap} reached"));
    }
    if reasons.is_empty() { None } else { Some(reasons.join("; ")) }
}

#[async_trait]
impl SearchBackend for LexicalBackend {
    fn id(&self) -> BackendId {
        BackendId::Lexical
    }

    fn supports(&self, kind: QueryKind) -> bool {
        let _ = kind;
        true
    }

    async fn search(&self, repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        let start = Instant::now();
        check_root(repo)?;
        let Some(query) = build_query(request)? else {
            return Ok(BackendAnswer::default());
        };
        let mut plan = self.plan(repo, query.matcher, start);
        plan.only_paths = request.paths.clone();
        let ranker = Arc::clone(&self.ranker);
        let config = self.config.clone();
        let repo = repo.clone();
        let query_text = request.query.clone();
        let fetch_limit = request.fetch_limit;
        let profile = query.profile;

        tokio::task::spawn_blocking(move || {
            let output = scan(plan)?;
            let partial = partial_reason(&output, config.max_candidates);
            let mut ranked = ranker.rank(&repo.root, &profile, !Ranker::exempt(&query_text), output.matches);
            ranked.truncate(config.max_candidates);

            let mut per_file = std::collections::HashMap::new();
            let mut hits = Vec::new();
            for item in ranked {
                let taken = per_file.entry(item.path.clone()).or_insert(0usize);
                if *taken >= config.max_hits_per_file {
                    continue;
                }
                *taken += 1;
                if hits.len() >= fetch_limit {
                    break;
                }
                let rank = u32::try_from(hits.len()).unwrap_or(u32::MAX);
                hits.push(Hit {
                    backend: BackendId::Lexical,
                    repo: repo.name.clone(),
                    path: item.path,
                    lines: LineRange::line(item.line),
                    symbol: if item.definition { profile.symbol().map(str::to_owned) } else { None },
                    symbol_kind: None,
                    is_definition: item.definition,
                    snippet: Some(item.text),
                    rank,
                    score: Some(item.score),
                });
            }
            Ok(BackendAnswer { hits, stale: None, partial })
        })
        .await
        .map_err(|err| BackendError::Failed(format!("lexical search task failed: {err}")))?
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        IndexState {
            backend: BackendId::Lexical,
            repo: repo.name.clone(),
            indexed: true,
            updated_at_ms: None,
            stale: None,
            detail: Some("no index needed".to_owned()),
        }
    }
}

/// The fallback source of `usages` for repositories without an ast-index index.
#[async_trait]
impl UsageSource for LexicalBackend {
    fn id(&self) -> BackendId {
        BackendId::Lexical
    }

    /// Whole-word, case-sensitive matches of the bare name, without its declaration lines.
    /// `complete` is false when `limit` or the deadline cut the list. When the limit cuts it,
    /// which usages are kept depends on the order the files were read; the kept ones are
    /// sorted by path and line.
    async fn usages(&self, repo: &Repo, symbol: &str, limit: usize) -> Result<UsageAnswer, BackendError> {
        let start = Instant::now();
        check_root(repo)?;
        let name = bare_name(symbol);
        if name.is_empty() {
            return Ok(UsageAnswer { usages: Vec::new(), complete: true, stale: None });
        }
        let matcher = RegexMatcherBuilder::new().fixed_strings(true).word(true).build(name).map_err(matcher_error)?;
        let mut plan = self.plan(repo, matcher, start);
        plan.skip_line = declaration_regex(name);
        plan.per_file_cap = usize::MAX;
        plan.global_cap = limit;
        let repo = repo.clone();

        tokio::task::spawn_blocking(move || {
            let output = scan(plan)?;
            let complete = !output.capped && !output.timed_out;
            let mut matches = output.matches;
            matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
            let usages = matches
                .into_iter()
                .map(|found| Usage {
                    repo: repo.name.clone(),
                    path: found.path,
                    line: found.line,
                    context: Some(found.text.trim().to_owned()),
                })
                .collect();
            Ok(UsageAnswer { usages, complete, stale: None })
        })
        .await
        .map_err(|err| BackendError::Failed(format!("lexical usages task failed: {err}")))?
    }
}
