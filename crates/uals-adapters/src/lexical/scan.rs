//! The parallel walk and the per-file search. Knows nothing about ranking: it returns raw
//! matches (path, line number, cut line) plus flags that say why it stopped early.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use grep_regex::RegexMatcher;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::{DirEntry, WalkBuilder, WalkState};
use regex::Regex;
use uals_core::backend::BackendError;

use crate::path_filter::PathFilter;

/// A line longer than this makes the searcher fail for that file (it is skipped, the query goes
/// on). `max_file_size_bytes` normally keeps files far below it.
const HEAP_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// One matched line.
#[derive(Debug)]
pub(super) struct RawMatch {
    pub path: PathBuf,
    /// 1-based.
    pub line: u32,
    /// The line without its terminator, cut to `max_line_chars` characters.
    pub text: String,
}

/// Everything one scan needs.
pub(super) struct ScanPlan {
    pub root: PathBuf,
    /// Gitignore-style globs relative to `root`; a file is searched only when one of them selects
    /// it. Empty means the whole tree.
    pub only_paths: Vec<String>,
    pub matcher: RegexMatcher,
    /// Lines for which this regex matches are neither counted nor collected.
    pub skip_line: Option<Regex>,
    /// The searcher stops reading a file after this many collected matches.
    pub per_file_cap: usize,
    /// The scan stops when a match beyond this many is found. Nothing beyond it is collected.
    pub global_cap: usize,
    pub max_filesize: u64,
    pub max_line_chars: usize,
    /// The scan stops by itself at this moment.
    pub stop_at: Instant,
}

/// What a scan found and why it stopped early, if it did.
pub(super) struct ScanOutput {
    pub matches: Vec<RawMatch>,
    /// A match beyond `global_cap` exists: the list is a subset.
    pub capped: bool,
    /// The scan stopped at `stop_at` before it covered every file.
    pub timed_out: bool,
}

/// The read-only part of a plan, shared by every worker thread.
struct Params {
    matcher: RegexMatcher,
    skip_line: Option<Regex>,
    per_file_cap: usize,
    global_cap: usize,
    max_line_chars: usize,
    stop_at: Instant,
}

/// The state the worker threads share: counters, stop flags and the collected matches.
struct Shared {
    total: AtomicUsize,
    stop: AtomicBool,
    capped: AtomicBool,
    timed_out: AtomicBool,
    out: Mutex<Vec<RawMatch>>,
}

/// Walks the tree in parallel and collects matches.
pub(super) fn scan(plan: ScanPlan) -> Result<ScanOutput, BackendError> {
    let walker = build_walker(&plan.root, &plan.only_paths, plan.max_filesize)?;
    let params = Arc::new(Params {
        matcher: plan.matcher,
        skip_line: plan.skip_line,
        per_file_cap: plan.per_file_cap.max(1),
        global_cap: plan.global_cap,
        max_line_chars: plan.max_line_chars,
        stop_at: plan.stop_at,
    });
    let shared = Arc::new(Shared {
        total: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        capped: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
        out: Mutex::new(Vec::new()),
    });
    walker.build_parallel().run(|| {
        let mut worker = Worker::new(Arc::clone(&params), Arc::clone(&shared));
        Box::new(move |entry| worker.visit(entry))
    });
    let matches = std::mem::take(&mut *shared.out.lock().unwrap_or_else(PoisonError::into_inner));
    Ok(ScanOutput {
        matches,
        capped: shared.capped.load(Ordering::Relaxed),
        timed_out: shared.timed_out.load(Ordering::Relaxed),
    })
}

/// The walker from spike S3: ignore files are honored even without `.git`, nothing above the root
/// is read, dotfiles are searched but `.git` never is, symlinks are not followed.
fn build_walker(root: &Path, only_paths: &[String], max_filesize: u64) -> Result<WalkBuilder, BackendError> {
    let mut builder = WalkBuilder::new(root);
    builder
        .require_git(false)
        .parents(false)
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .ignore(true)
        .add_custom_ignore_filename(".rgignore")
        .max_filesize(Some(max_filesize));

    let mut overrides = OverrideBuilder::new(root);
    overrides.add("!.DS_Store").map_err(|err| BackendError::Failed(format!("ignore override: {err}")))?;
    builder.overrides(overrides.build().map_err(|err| BackendError::Failed(format!("ignore override: {err}")))?);

    // `paths` cannot be override globs: a whitelist override beats `.gitignore`, so `src/**`
    // would bring back `src/build/`. A separate filter only selects among the walked files.
    let selector = PathFilter::new(only_paths)?;
    let root = root.to_path_buf();
    builder.filter_entry(move |entry| {
        if entry.file_name() == ".git" {
            return false;
        }
        if selector.is_active() && entry.file_type().is_some_and(|kind| kind.is_file()) {
            return selector.allows(entry.path().strip_prefix(&root).unwrap_or(entry.path()));
        }
        true
    });
    Ok(builder)
}

/// One walker thread: a searcher, a matcher clone and a local list of matches that is handed to
/// the shared list when the thread ends.
struct Worker {
    params: Arc<Params>,
    shared: Arc<Shared>,
    searcher: Searcher,
    local: Vec<RawMatch>,
}

impl Worker {
    fn new(params: Arc<Params>, shared: Arc<Shared>) -> Self {
        let searcher = SearcherBuilder::new()
            // `quit`, not `convert`: converting NUL to a line break shifts the line numbers.
            .binary_detection(BinaryDetection::quit(b'\0'))
            .heap_limit(Some(HEAP_LIMIT_BYTES))
            .build();
        Self { params, shared, searcher, local: Vec::new() }
    }

    /// True once the scan was told to stop or the deadline has passed (the latter also marks the
    /// scan as timed out).
    fn should_stop(&self) -> bool {
        if self.shared.stop.load(Ordering::Relaxed) {
            return true;
        }
        if Instant::now() >= self.params.stop_at {
            self.shared.timed_out.store(true, Ordering::Relaxed);
            self.shared.stop.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Searches one walked file; a stop request ends the walk.
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        if self.should_stop() {
            return WalkState::Quit;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(%err, "lexical: skipping a walk entry");
                return WalkState::Continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            return WalkState::Continue;
        }
        self.search_file(entry.path());
        if self.shared.stop.load(Ordering::Relaxed) { WalkState::Quit } else { WalkState::Continue }
    }

    /// Collects the matches of one file into the local list. A file that cannot be read is
    /// logged and skipped.
    fn search_file(&mut self, path: &Path) {
        let Self { params, shared, searcher, local } = self;
        let mut in_file = 0usize;
        let result = searcher.search_path(
            &params.matcher,
            path,
            Lossy(|line_number, line| {
                let deadline_passed = Instant::now() >= params.stop_at;
                if shared.stop.load(Ordering::Relaxed) || deadline_passed {
                    if deadline_passed {
                        shared.timed_out.store(true, Ordering::Relaxed);
                    }
                    shared.stop.store(true, Ordering::Relaxed);
                    return Ok(false);
                }
                if params.skip_line.as_ref().is_some_and(|skip| skip.is_match(line)) {
                    return Ok(true);
                }
                if shared.total.fetch_add(1, Ordering::Relaxed) >= params.global_cap {
                    shared.capped.store(true, Ordering::Relaxed);
                    shared.stop.store(true, Ordering::Relaxed);
                    return Ok(false);
                }
                local.push(RawMatch {
                    path: path.to_path_buf(),
                    line: u32::try_from(line_number).unwrap_or(u32::MAX),
                    text: cut_line(line, params.max_line_chars),
                });
                in_file += 1;
                Ok::<bool, io::Error>(in_file < params.per_file_cap)
            }),
        );
        if let Err(err) = result {
            tracing::warn!(path = %path.display(), %err, "lexical: skipping a file that could not be searched");
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if !self.local.is_empty() {
            self.shared.out.lock().unwrap_or_else(PoisonError::into_inner).append(&mut self.local);
        }
    }
}

/// Drops the line terminator and keeps at most `max_chars` characters.
fn cut_line(line: &str, max_chars: usize) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    match line.char_indices().nth(max_chars) {
        Some((index, _)) => line[..index].to_owned(),
        None => line.to_owned(),
    }
}
