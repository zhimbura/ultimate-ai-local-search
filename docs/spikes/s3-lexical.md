# Spike S3: in-process lexical search over a non-git parent folder

Status: measured. Spike code lives outside this repo (standalone cargo project, not committed).
Measured on macOS (arm64), release build, 2026-11-12.

Crates (latest at spike time, from `Cargo.lock`): `ignore 0.4.34`, `grep-searcher 0.1.17`,
`grep-regex 0.1.14`, `grep-matcher 0.1.9`. rustc/cargo 1.99.1. Reference: ripgrep CLI.

Test corpora:

- Public fixture `tests/fixtures/workspace` (3 projects), copied to a temp dir, `git init` per project,
  plus junk (`app/build/gen/X.kt`, `node_modules/x/index.ts`), one untracked non-ignored file, one hidden file.
- A private workspace of 4 Gradle repos (Kotlin/Java), called "private repo 1-4" below. It sits in a parent
  folder that is NOT a git repo. Aggregate: 8,259 files walked, about 100 MB of text; on disk the same repos
  hold about 632k files under 179 `build/` directories (all excluded by the walker).

## 1. Ignore rules from a non-git parent

Answer: yes. Each child repo's `.gitignore` is honored, because `ignore` evaluates `.git` presence per
directory, not only at the walk root. A parent without `.git` is not a problem.

| Config | Fixture result (3 repos, 22-26 files) | Private workspace (4 repos) |
|---|---|---|
| default `WalkBuilder::new(parent)` | `build/` and `node_modules/` junk excluded; hidden files skipped (`.gitignore`, `.hiddenfile` missing vs git) | 7,695 files, 0 under `build/`; hidden files skipped (-201/-132/-121/-117 vs `git ls-files` per repo, almost all hidden or symlinks) |
| `.require_git(false)` | same as default | same count |
| `.parents(false)` | same as default | same count |
| `.git_global(false)` / `.git_exclude(false)` | same | `git_global(false)` adds 1 file (matched only by the user's global ignore); `git_exclude(false)` adds 1 file |
| `.hidden(false)` alone | walks INTO `.git/` (127 extra files on the fixture) | not measured |
| `.hidden(false)` + `filter_entry` skipping `.git` | exact match with git (see below) | see below |

Where the flags matter:

- `require_git(false)`: needed only when a project has `.gitignore` but no `.git` (exported copy, tarball,
  CI checkout). Fixture without `.git` dirs: default walked `build/gen/X.kt` and `node_modules/x/index.ts`
  (not ignored); with `require_git(false)` both were excluded. Cheap safety net, so set it.
- `parents(false)`: stops the walker from reading ignore files ABOVE the root. The root's own `.gitignore`
  is still applied (parent with its own `.gitignore` ignoring `sample-web/`: 0 files from it in every config).
  Set it so a stray `.gitignore` above the workspace cannot silently hide repos.
- `git_global`: with a global excludes file containing `*.ts`, default/`git_global(true)` dropped all 3 `.ts`
  files; `git_global(false)` kept them. Default is true and equals `git ls-files -o --exclude-standard`
  semantics. Keep true (parity with git and rg); the cost is result variance across machines.
- `hidden(false)`: required to search dotfiles that matter (`.github/`, `.gitlab-ci.yml`, `.gitignore`).
  Then the walker MUST skip `.git` (directory, or file in worktrees/submodules) with `filter_entry`.

Per-repo parity vs `git ls-files` + `git ls-files -o --exclude-standard` with the recommended config:

- Fixture: walked == git for all 3 repos (15/15, 5/5, 5/5, extra 0, missing 0).
- Private workspace: extra 0 in all 4 repos. Missing: 21 / 25 / 17 / 18 files per repo (81 total), all of them
  either (a) tracked symlinks (16 per repo, `.claude/skills/*` links to dirs; not followed) or (b) files that
  are tracked but also match `.gitignore` (21 in repo 1, e.g. force-added files; git lists them, ignore does not).
  Both are intended: symlinks are skipped, ignore rules win over tracking (same as rg).
- Files under any `build/` directory: 0 (of ~632k on disk).
- Parent-level noise (outside any repo): about 80 files (`.idea/`, `.claude/`, `CLAUDE.md`, `.DS_Store`).

Recommended config:

```rust
use ignore::{WalkBuilder, overrides::OverrideBuilder};

fn workspace_walker(root: &std::path::Path) -> Result<WalkBuilder, ignore::Error> {
    let mut b = WalkBuilder::new(root);
    b.require_git(false)      // honor .gitignore even where .git is absent
        .parents(false)       // never read ignore files above the workspace root
        .hidden(false)        // search dotfiles / dot-dirs ...
        .follow_links(false)  // symlinks are not followed (no loops, no escapes)
        .git_ignore(true)
        .git_exclude(true)    // .git/info/exclude, like `git ls-files --exclude-standard`
        .git_global(true)     // parity with git/rg; set false for machine-independent results
        .ignore(true)         // .ignore files (rg compatible)
        .add_custom_ignore_filename(".rgignore") // the crate does NOT read .rgignore by default
        .max_filesize(Some(2 * 1024 * 1024))
        .filter_entry(|e| e.file_name() != ".git"); // ... but never walk into .git
    let mut ov = OverrideBuilder::new(root);
    ov.add("!.DS_Store")?;    // plus optional parent-level "!/.idea/" if only repos matter
    b.overrides(ov.build()?);
    Ok(b)
}
```

Note: `filter_entry` and `overrides` are applied on every directory entry; both were checked to cost nothing
measurable (walk 100-120 ms either way).

## 2. Speed and parity (private workspace, 8,259 files, about 100 MB)

Parallel walk (`build_parallel`, default thread count) with one `Searcher` + `RegexMatcher` clone per thread.

| Query | Files scanned | Matches | Match files | Wall (warm, 5 runs) | Peak RSS |
|---|---|---|---|---|---|
| fixed string, class name, whole word | 8,259 | 35 | 15 | 245-297 ms | 10.8 MB |
| fixed string, same, substring | 8,259 | 109 | 33 | 242-250 ms | 10.8 MB |
| regex `class \w+<Suffix>` | 8,259 | 10 | 9 | 246-255 ms | 11.8 MB |
| smart case / `-i` lowercase of the identifier | 8,259 | 132 | 33 | 242-293 ms | 11 MB |

- Walk alone (single thread, no reads): 101-168 ms (first run 168, then about 105).
- `rg` on the same query and equivalent flags: 250-260 ms wall. In-process is on par with the CLI.
- Parity with `rg --json` (flags `--hidden -g '!.git' --no-require-git --no-ignore-parent --no-config`):
  identical sets of (path, line number) for 6 queries (fixed, fixed+word, regex, smart case lower, smart case
  upper, case-insensitive): 109, 35, 10, 132, 109, 132 hits, 0 diffs. Order differs (parallel walk), so sort
  by path then numeric line before comparing; a string sort of `path:line` falsely looks like a diff.
- Cold cache: not measured. `purge` needs sudo, unavailable here. The first search after a rebuild
  (partially cold) took 382 ms for a fixed-string query, i.e. cold is expected within about 1.5x of warm
  for this 100 MB set. Re-measure on a machine where `purge` is allowed.

## 3. Matching options

All verified on the private workspace unless marked "synthetic".

- Smart case: `RegexMatcherBuilder::case_smart(true)`; lowercase pattern gave 132 hits (case-insensitive),
  mixed-case pattern gave 109 (sensitive); identical to `rg -S`.
- Whole word: `.word(true)`; 109 -> 35 hits, identical to `rg -w`.
- Fixed strings: `.fixed_strings(true)`; same hits as the escaped regex, no measurable speed difference at
  this size (the regex engine already extracts literals).
- Binary detection: `BinaryDetection::quit(0)` matches rg: 302 of 8,259 files (3.7%) have NUL in the first
  64 KB and are dropped entirely; NUL after the first buffer keeps earlier matches. Synthetic: NUL at start
  dropped, NUL after a first matching line (small file) dropped. `BinaryDetection::none()` reports matches inside
  binaries. Do NOT use `convert(0)`: NUL becomes `\n`, so line numbers shift (synthetic: line 4 reported
  for a match on line 1).
- Max file size: `WalkBuilder::max_filesize`. Private workspace: 10 files > 1 MB, 32 files > 256 KB,
  largest 9.4 MB (a data file). 256 KB cap skipped 32 files, 1 MB cap skipped 10; peak RSS 7.1 MB vs 10.7 MB.
  Recommendation 2 MB.
- Early stop per file: return `Ok(false)` from `Sink::matched`. Fixed-string query with cap 1: 15 hits (one per
  matching file), cap 3: 29 of 35. The search stops reading that file.
- Early stop global: shared `AtomicUsize` + `WalkState::Quit`. Cap 10 stopped after 1,956 of 8,259 files in
  53 ms with 12 hits (soft cap: overshoot up to threads x per-file cap, so truncate after sorting).
  Query `e` with per-file 5 and global 200: 42 files, 204 hits, 1.8 ms.
- Memory: about 11 MB peak for selective queries. Unbounded query `.`: 841,409 matches collected, 437 MB peak.
  So a global cap and truncating the stored line (for example 300 chars) are mandatory.
  Query `e` with per-file cap 5: 38,247 hits, 38 MB.

## 4. Down-ranking hints (counts over the 8,259 searchable files)

| Pattern | Files |
|---|---|
| `/build/` | 0 (already ignored) |
| `/generated/` | 7 (baseline-profile output, tracked) |
| `kapt` / `ksp` directories | 0 |
| `/test/` | 358 |
| `/androidTest/` | 27 |
| `/commonTest/`, `/jvmTest/`, `/androidUnitTest/`, `/androidInstrumentedTest/`, `/iosTest/` | 67 |
| `/testFixtures/` | 1 |
| `*Test.kt`, `*Test.java` | 408 |
| `*Tests.kt` | 0 |
| union of all test patterns above | 461 (5.6%) |
| `Fake*`, `Mock*`, `Stub*` files | 15 |
| `/sandbox/` | 131 |
| `/mock(s)/`, `/fixtures/`, `/testdata/`, `/samples/` | 3 |
| `*_Impl`, `*_Factory`, `Dagger*`, `Hilt_*` | 0 |
| `BuildConfig.*`, `R.java` | 0 |
| `*.lock`, `*.min.js`, `*.map` | 0 |
| `res/values-xx/strings.xml` (localized resources) | 993 (12%) |
| `*.md` | 450 |
| `.idea/`, `.claude/`, `.agents/` directories | 452 |
| `/ci/`, `/scripts/` | 109 |

Proposed down-rank list (multiplicative penalty, not exclusion; exclusion would break "find the test of X"):

1. Strong (x0.3): `(^|/)build/`, `(^|/)generated/`, `/(kapt|ksp)[^/]*/`, `/\.gradle/`, `/node_modules/`,
   `\.min\.(js|css)$`, `\.(lock|map)$`, `_Impl\.`, `_Factory\.`, `Hilt_`, `Dagger[A-Z]`, `\$\$`.
2. Medium (x0.5): `/test/`, `/androidTest/`, `/(common|jvm|android|ios|desktop)[A-Za-z]*Test/`,
   `/testFixtures/`, `Tests?\.(kt|java|ts|py)$`, `/(Fake|Mock|Stub)[A-Z]`, `/sandbox/`, `/(mocks?|fixtures?|testdata)/`.
3. Weak (x0.7): `res/values-[a-z]{2}(-r[A-Z]{2})?/strings\.xml`, `\.md$`, `(^|/)\.(idea|claude|agents)/`.
4. Invert for queries that contain `test`/`mock`/`fake` tokens (do not penalise when the user asks for tests).

## 5. Gotchas

- Symlinks: not followed by default; tracked symlinks to directories (16 per repo here) are silently absent,
  matching rg. `follow_links(true)` followed a file link (found a duplicate hit) and survived a self-referencing
  directory link without crashing (loop detected, error returned per entry), but it can leave the workspace;
  keep it off.
- `.ignore` is honored by default; `.rgignore` is NOT (synthetic: file listed in `.rgignore` was still
  searched) until `add_custom_ignore_filename(".rgignore")`. `.gitignore` honored.
- Non-UTF-8: 0 non-UTF-8 text files here. Synthetic: Latin-1 file matches (bytes engine, ASCII pattern);
  UTF-16 with BOM matches (BOM sniffing on by default). Convert with `String::from_utf8_lossy` for output.
- Very long lines: line-oriented searching buffers a whole line. 42 files have lines > 10 KB, longest 786 KB.
  Synthetic 100 MB single-line file: match found, peak RSS 333 MB, 83 ms. `Searcher::heap_limit` turned this into a
  per-file error (file skipped, no crash). The adapter must treat per-file errors as skips, not abort the query.
  `max_filesize` already prevents it; also truncate returned lines.
- Windows: normalize separators to `/` in results; case-insensitive FS means
  `.gitignore` case semantics differ (`ignore` handles it via `core.ignorecase` only partially);
  `.git` can be a file (worktrees, submodules): `filter_entry` on the name covers both; avoid long-path (>260)
  failures by catching per-entry errors; CRLF files: `$` anchors need `(?-u)\r?$` or `.crlf(true)` on
  `RegexMatcherBuilder`. Not tested on Windows in this spike.
- Parent-level files (`.idea`, `.claude`, `.DS_Store`, top-level docs) sit outside every repo and are walked.
  Decide whether repo roots are discovered first (`.git` child dirs) and only those are searched.
- Walk errors (permission, vanished file, broken symlink) arrive as `Err` items; log and continue.

## Recommended defaults

| Setting | Value | Reason |
|---|---|---|
| max hits per file | 10 | one file should not flood ranking; 5-10 covers real usage |
| global candidate cap | 500 hits (soft; truncate after rank) | memory and fusion cost; cap 10 stops in 53 ms |
| stored line length | 300 chars | memory, minified files (lines up to 786 KB) |
| max file size | 2 MB | skips 10 of 8,259 files, avoids pathological lines |
| query deadline | 3 s (check an `AtomicBool` flag in the walk closure, return `WalkState::Quit`) | full scan is about 0.25 s; 10x margin for cold disk |
| threads | `WalkBuilder` default | tuning not needed at this size |
| binary detection | `quit(0)` | rg parity |
| matcher | `case_smart(true)`, `fixed_strings` when the query has no regex metachars | parity with rg `-S` |

## Regression tests the real adapter should have

1. Non-git parent with 2 child repos: child `.gitignore` honored; zero files from `build/`, `node_modules/`.
2. Child dir with `.gitignore` but no `.git`: ignore rules still honored (`require_git(false)`).
3. Parent with its own `.gitignore` and stray ignore files above the root: not applied beyond root (`parents(false)`).
4. `.git` directory and `.git` file (worktree) are never walked or searched.
5. Hidden files (`.gitignore`, `.github/...`) are searched; `.DS_Store` is not.
6. Parity: for a fixed corpus and 5 queries (fixed, regex, word, smart case, case-insensitive) the (path, line) set
   equals `rg --json` with the equivalent flags (sort numerically before comparing).
7. Walked set equals `git ls-files` + `git ls-files -o --exclude-standard` minus tracked symlinks and
   tracked-but-ignored files (explicit expected-diff list).
8. Global gitignore: result is deterministic given `git_global` setting (test with `HOME` pointing at a temp config).
9. Binary file with NUL in first buffer is skipped; file with NUL later keeps earlier matches; no `convert`
   mode (line numbers must not shift).
10. Latin-1 and UTF-16-BOM files match; output is lossy-decoded, never panics.
11. 100 MB single-line file: skipped by `max_filesize`; with the cap lifted and `heap_limit` set, per-file error
    is a skip, query still succeeds.
12. Per-file cap and global cap are honored; global overshoot is bounded by threads x per-file cap; results
    are truncated to the cap after sorting; stored lines are truncated.
13. Deadline: a query with a 1 ms deadline returns partial results flagged `truncated`, no hang.
14. Symlink to a file and to a directory outside the workspace, plus a symlink loop: nothing outside is read, no hang.
15. `.ignore` honored, `.rgignore` honored (custom filename), per-file errors (permission denied, vanished file)
    are logged and skipped.
16. Down-ranking: a test-path hit ranks below a main-source hit of equal score; not penalised when the query
    contains `test`.
17. Unbounded query (`.`) stays under a memory budget (for example 100 MB) thanks to caps.
