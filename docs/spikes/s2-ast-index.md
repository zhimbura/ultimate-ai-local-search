# Spike S2: ast-index CLI as a uals backend

Tested against `ast-index 3.56.0` on the public fixture workspace (`tests/fixtures/workspace`).
Raw outputs are recorded in `crates/uals-adapters/tests/fixtures/ast_index/` by
`scripts/record-ast-fixtures.sh` (`<name>.json|.txt` = stdout, `<name>.meta.json` = `{cmd, args, project, exit_code, stderr, duration_ms}`).
Absolute temp paths are replaced by `$WORKSPACE` (the project tree) and `$TMPROOT` (the temp root holding the cache dir); the cache-hash directory is replaced by `<cache-hash>`.

## 1. Conventions common to every command

- Run with cwd = the project root (a subdirectory works too, see gotchas). Set `AST_INDEX_CACHE_DIR` to keep the cache out of `~/Library/Caches/ast-index`.
- All paths in results are relative to the project root with `/` separators, except the ones listed in "Absolute paths" below.
- Lines are 1-based. `end_line` is inclusive. A one-line symbol has `line == end_line`.
- Exit code is 0 for every command, including "no results", "index missing" and "unreachable". The only non-zero exit seen: `query` with a non-read-only statement (exit 1, stderr `Error: Only SELECT, WITH, and EXPLAIN queries are allowed`, empty stdout).
- Diagnostics (`Loaded N files`, `Found N new/changed`) go to stderr; data goes to stdout.
- Key order in JSON is not stable across commands (some are alphabetical). Always parse by key.

Absolute paths appear only in: `stats.db_path`, `db-path` (plain text), `query` over `metadata` (`project_root`), and `query` over `files` (`root_path`).

## 2. Per-command JSON shapes

`pagination` is `{total, returned, truncated, limit}`. Default limits: symbol-like 20, usages/callers 50, query 100 (`--limit`).

| Command | Shape |
|---|---|
| `stats` | `{db_path, db_size_bytes, project, stats:{file_count, symbol_count, module_count, refs_count, resources_count, xml_usages_count, ios_assets_count, storyboard_usages_count}}` |
| `symbol X`, `symbol X --fuzzy`, `symbol -p '*Store'`, `class X`, `implementations X` | `{schema_version:2, items:[{name, kind, line, signature, path}], pagination}`. `kind` examples: `class`, `interface`, `function`, `property`, `import` (Python imports are returned as symbols of kind `import`). No `end_line` here. `-p` takes a glob. Plain `symbol hostsel` also matched `HostSelector` (substring/case-insensitive match), so `--fuzzy` made no visible difference on this fixture. |
| `usages X` | `{schema_version:2, items:[{name, line, context, path, test?}], pagination}`. `test:true` is present only on test files (absent otherwise). `context` is the trimmed source line. |
| `callers f` | `{schema_version:2, items:[{content, line, path}], pagination}`. Grep-based: `content` keeps the original indentation. |
| `refs X` | `{schema_version:2, definitions:[{kind,line,name,path,signature}], imports:[...], usages:[{context,line,name,path,test?}], pagination:{definitions,imports,usages}}` |
| `search X` (single word) | `{schema_version:2, symbols:[{kind,line,name,path,signature}], files:[path], content_matches:[{content,line,path}], references:[{name,usage_count}], pagination:{symbols,files,content_matches,references}}` |
| `search "a b"` (multi word, no literal hit) | NOT the shape above. Falls back to the `explore` shape plus `fallback:"explore"` and `reason`. The adapter must branch on `fallback`. |
| `explore ...` | `{dominant_language, query, symbols:[{name,kind,line,path,score,vendor}], files:[{path,symbol,line,outline:[{name,kind,line,end_line}],outline_hidden}], neighbours:[], tests:[{source, tests:[path]}]}`. `files[]` may carry `source` instead of `outline` (web/Python fixtures). `score` is a float, no upper bound seen (57.8). No `schema_version`, no `pagination`. Quality is weak on tiny projects: `explore choose backend url` returned a single symbol `get`. |
| `outline <file>` | `{file, schema_version:1, symbols:[{name,kind,line,end_line}]}` (flat list, nesting is not expressed; members follow their parent by line order). The file argument is a project-relative path. |
| `file X` | A bare JSON array of path strings (no wrapper, no pagination). |
| `module-route --from a --to b` | `{from, to, paths:[{hops:[{from,to,kind}], length}], count, truncated}`; when nothing is found also `empty_reason`: `unreachable` (modules exist, no path) or `index_missing` (see section 5). |
| `schema` | `{<table>:{columns:[{name,type,not_null,primary_key}], row_count}}` |
| `query "<SQL>"` | `{columns:[names in SELECT order], count, rows:[{col:value}]}`. Row objects are key-sorted, so use `columns` for order. SQL NULL is JSON `null` (e.g. `modules.kind`). Read-only only. |

## 3. Text-only commands (ignore `--format json`)

`deps`, `dependents`, `hierarchy` print human text even with `--format json` (recorded as `.txt`, exit 0):

```
Dependencies of 'network' (1):
  implementation:
    core (core)
```
```
Modules depending on 'core' (2):
  via implementation (2):
    app (app)
    network (network)
```
```
Hierarchy for 'HostStore':

  Children (1):
    InMemoryHostStore [class]: network/src/main/kotlin/sample/network/HostStore.kt
```

Do not parse these. Replace with SQL over `query` (section 6): module edges from `module_deps`+`modules`, children from `inheritance`+`symbols`.
Also text-only: `db-path` (one line, absolute path of `index.db`), `update`, `rebuild`.

## 4. Other projects (sample-web, sample-tools)

Same shapes as Android. Notes:
- TypeScript: `export async function fetchWithRetry(` yields `kind:"function"`, signature includes `export async`. Re-exports in `index.ts` surface as symbols in `explore`.
- Python: `LruCache` yields 2 symbols for one name: the `class` and an `import` row (`from sample_tools.cache import LruCache` in `__init__.py`). The adapter should drop `kind == "import"` for definition lookups. Methods such as `__init__`, `get`, `put` appear in `outline` as `function`.
- `explore` `symbols[]` may contain import-like names (`../http/fetchWithRetry`, `sample_tools.cache`).
- Modules: sample-web and sample-tools have 0 modules (`module-route` is Gradle-specific).

## 5. Missing index

Index absent (copy of `sample-android` never rebuilt, cache dir empty):

| Command | stdout | stderr | exit |
|---|---|---|---|
| `stats --format json` | `Index not found. Run 'ast-index rebuild' first.` (plain text) | empty | 0 |
| `symbol X --format json` | same plain text | empty | 0 |
| `module-route ... --format json` | JSON with `paths:[]`, `empty_reason:"index_missing"` | empty | 0 |
| `query "SELECT 1"` | valid JSON (an in-memory DB with no tables answers) | empty | 0 |

So there is no machine-readable error and no non-zero exit. Detection rule for the adapter: if stdout does not start with `{` or `[` (or starts with `Index not found`), treat as `IndexMissing`. For `module-route`, check `empty_reason == "index_missing"`. `query` on a real table fails with `no such table: files` (stderr/exit not recorded as a fixture). Do not use `query "SELECT 1"` to probe for existence; use `stats` and look for the sentinel text (the `stats` JSON shape is also stable: `file_count` > 0).
No auto-rebuild happens on a query. The adapter must call `ast-index rebuild` itself (about 160-180 ms on the fixtures).

## 6. SQL the adapter should use

Tables (from `schema`): `files(id,path,root_path,mtime,size)`, `symbols(id,file_id,name,qualified_name,kind,line,end_line,parent_id,signature)`, `modules(id,name,path,kind)`, `module_deps(id,module_id,dep_module_id,dep_kind)`, `transitive_deps`, `inheritance(id,child_id,parent_name,kind)`, `refs(id,file_id,name,line,context)`, `metadata(key,value)`. `symbols` has NO path column; join `files`.

Symbol ranges (replaces per-file `outline`, one call for many files):

```sql
SELECT f.path AS path, s.name, s.kind, s.line, s.end_line
FROM symbols s JOIN files f ON f.id = s.file_id
WHERE f.path IN ('<rel path 1>', '<rel path 2>')
ORDER BY f.path, s.line
```

`parent_id` and `qualified_name` are available for nesting if needed (not exercised here).

Module dependency edges (replaces `deps`; swap the WHERE for `dependents`):

```sql
SELECT m.name AS module, d.name AS depends_on, md.dep_kind
FROM module_deps md
JOIN modules m ON m.id = md.module_id
JOIN modules d ON d.id = md.dep_module_id
WHERE m.name = 'network'      -- forward: edges out of network
-- WHERE d.name = 'core'      -- reverse: who depends on core
ORDER BY d.name
```

Fixture results: `network -> core [implementation]`; reverse of `core`: `app`, `network` (both `implementation`).
`dep_kind` is a string (`implementation`, matches `module-route` hop `kind`).

Subtypes (replaces `hierarchy`):

```sql
SELECT s.name AS child, i.parent_name, i.kind
FROM inheritance i JOIN symbols s ON s.id = i.child_id
WHERE i.parent_name = 'HostStore'
```

`inheritance.kind` is `implements` here. Use `symbols.signature`/`path` joins for locations.

Sanitise user-provided strings: `query` takes a raw SQL string (no parameter binding), so only inline values that were validated (identifier/path charset) and escape `'` as `''`.

## 7. Staleness

Nothing signals a stale index. Verified after editing `HostSelector.kt`, adding `NewThing.kt` and changing `core/build.gradle.kts` without `update`:

- `symbol NewThing` returns an empty `items` (exit 0), `outline` returns the old line ranges, `module-route` shows no warning, `stats` is unchanged. No stderr, no flag.
- `metadata` is unchanged: `last_update_at` (milliseconds since epoch), `index_generation`, `build_files_fingerprint` stay the same.
- `files.mtime` is seconds since epoch and `files.size` is bytes, stored per indexed file at index time.

After `ast-index update` (stdout `Checking for changes...` / `Updated: 2 files ...`, stderr `Loaded 7 files from index` / `Found 2 new/changed files, 0 deleted files`; about 80-90 ms): `NewThing` is found, `index_generation` goes 1 to 2, `build_files_fingerprint` changes, `last_update_at` advances. A second `update` reports 0 changed and still costs ~90 ms.

Reliable detection for the adapter, cheapest first:
1. Changed or deleted files: `SELECT path, mtime, size FROM files`, then `stat` each file under the root and compare `mtime` (seconds) and `size`. Any mismatch or missing file means stale.
2. New files: not visible from the index; cover with `git status --porcelain` / `git ls-files -o --exclude-standard` (the fixture projects are git repos) or a bounded directory walk newer than `last_update_at`.
3. Build files: `metadata.build_files_fingerprint` changes on `update`; a mismatch can only be seen by recomputing it, so rely on 1 (build files are indexed files too) or just run `update`.
4. Simplest robust policy: run `ast-index update` before a query batch (idempotent, ~80-90 ms, incremental), or only when 1 or 2 reports drift. Compare `last_update_at` only as a freshness label (it records when the last update ran, not whether files changed since).

## 8. Latency (macOS, warm FS cache, fixture projects, single run, wall clock including process spawn)

| Command | ms |
|---|---|
| `rebuild` (sample-android / sample-web) | ~160 / ~180 |
| `update` (2 changed files / no-op) | 77 / 91 |
| `stats` | 34 |
| `symbol` | 38 (first call), 10 (later) |
| `symbol --fuzzy`, `symbol -p` | 56, 46 |
| `class`, `file`, `implementations`, `usages`, `refs`, `callers`, `search` | 10-23 |
| `explore` | 14 |
| `outline` | 17 |
| `module-route` | 11 |
| `query` (any of the recorded) | 9-12 |
| `deps` / `dependents` / `hierarchy` (text) | 10-11 |
| `stats` from a subdirectory | 78 |
| probe in a marker-less folder | 298 |

Fixture projects are tiny (7 files), so these are floor values dominated by process start. Per-call figures are in each `*.meta.json`.

## 9. Gotchas

- Root detection walks UP from cwd to the nearest project marker (observed: `settings.gradle.kts` for Gradle; marker list otherwise not enumerated). Running in `sample-android/network` used the `sample-android` index (`stats_from_subdir`, `symbol_from_subdir`) and paths stayed relative to `sample-android`, not to cwd. Always spawn with cwd = the intended project root and map result paths from that root.
- In the marker-less workspace root (`probe_workspace_root_stats`), nothing is found (`Index not found...`, exit 0) and it is the slowest call (298 ms).
- Probing creates stub cache directories: each probe of a folder (and its ancestors) creates `<cache>/<hash>/` containing only `.ast-index-generation-v1.json` and `.ast-index-owner-v1.json` (8 stubs after one probe of the workspace root plus a missing-index run). A stub is NOT an index (no `index.db`). Never treat directory existence as "indexed". Pre-check `<cache>/<hash>/index.db` instead, or use `db-path` plus a file-exists test, and keep the adapter's cache dir dedicated so stubs do not litter the user cache.
- The cache dir name is a hash of the absolute root path; a moved or renamed project gets a new, empty index.
- `search` changes shape for multi-word queries (section 2) and `explore` has no `schema_version`.
- Python `import` rows pollute `symbol` results (section 4).
- `symbol` results carry no `end_line`; use the `query` SQL for ranges.
- `rebuild` prints progress on stdout/stderr; discard it and check the exit code plus a follow-up `stats`.
- `metadata` values are all strings (even `last_update_at`).
- Fixtures contain run-specific values: timestamps, `duration_ms`, `db_size_bytes`. Tests should not assert them.
