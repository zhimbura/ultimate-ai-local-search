# S1 spike: driving Semble and claude-context as MCP clients with rmcp 3.9.0

Scope: `rmcp` 3.9.0 (features `client`, `transport-child-process`) as a stdio MCP **client** against
Semble 0.7.3 and claude-context 0.1.15. All measurements were made on macOS with the public
fixture workspace (`tests/fixtures/workspace`, copied to a fresh directory with `git init` in each of the three
projects). In this document the fixture copy is `$WORKSPACE`.

Recordings live in `crates/uals-adapters/tests/fixtures/semble/` and `.../claude_context/`: `<name>.json` is the
serde-serialized `CallToolResult` (or, for non-results, a small error object, see "Fixture conventions"),
`<name>.meta.json` is `{server, tool, arguments, duration_ms, notes}`.

Milvus was DOWN during the whole spike (`127.0.0.1:19530` refused, `:9091/healthz` unreachable), so **the
claude-context happy path (index, status poll, search, clear) was not recorded**. Everything said about
claude-context output formats below is read from its source (`dist/handlers.js`) and marked "source".

## Summary of answers

| # | Question | Answer |
|---|---|---|
| 1 | Handshake | Works with both. Client offers `2026-07-28`; both servers answer `2025-11-25`; rmcp accepts the downgrade. No non-JSON on stdout (0 of ~60 wire lines). Semble stderr is silent; claude-context stderr is ~55 debug lines at startup. |
| 2 | Tools | Semble: `search`, `find_related`. claude-context: `index_codebase`, `search_code`, `clear_index`, `get_indexing_status`. Raw lists are fixtures `tools_list`. |
| 3 | Semble `search` | Text + `structuredContent`, both carry the same string `{"result": "<json string>"}`. Inner JSON: `{query, results:[{file_path,start_line,end_line,score,content?}], repos?}`. |
| 4 | Errors | Semble reports bad paths as **`isError:false`** plain text. claude-context with Milvus down **crashes the server process** (see below). |
| 5 | Concurrency | One connection takes concurrent calls fine. Semble warm searches serialize (event loop), cold index builds overlap. |
| 6 | Lifecycle | `ProcessGroup::leader()` works. Semble exits on stdin EOF. claude-context ignores EOF; rmcp kills it only after 3 s. Orphan risk if the parent dies. |
| 7 | Cold start | Semble init 0.9-1.5 s (first ever 2.1 s); first cold search 0.3-1.1 s depending on repo size. claude-context init 0.45-1.0 s. |
| 8 | Cancellation | Dropping a `call_tool` future makes rmcp send `notifications/cancelled`; connection stays usable. |

## 1. Handshake

Wire log (a stdio tee proxy between rmcp and the server, `wire_proxy.py`, kept in the spike project):

```
C->S {"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{},"clientInfo":{"name":"rmcp","version":"3.9.0"},"protocolVersion":"2026-07-28"}}
S->C {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-11-25", ...}}
C->S {"jsonrpc":"2.0","method":"notifications/initialized"}
C->S {"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"progressToken":0}}}
```

| | Semble 0.7.3 | claude-context 0.1.15 |
|---|---|---|
| negotiated protocol | `2025-11-25` | `2025-11-25` |
| serverInfo | `{"name":"semble","version":"0.7.3"}` | `{"name":"Context MCP Server","version":"1.0.0"}` (not the package version) |
| capabilities | `tools`, `prompts`, `resources` (all `listChanged:false`) | `tools: {}` only |
| `instructions` | yes (usage hints for LLMs) | none |
| spawn to initialize done | 0.92-1.48 s (first run after install 2.08 s) | 0.45-1.0 s |
| stdout non-JSON lines | 0 | 0 |
| stderr at startup | empty. Later one line per rejected call: `Tool 'search' rejected arguments: ['top_k']`, `Tool 'no_such_tool' failed: ...` | about 55 lines: env debug (API key length only, not the key), embedding provider setup, snapshot load/save, `[SYNC-DEBUG]` lines. Logs also appear at +5 s (initial background sync) and every 5 min. |

Notes:
- rmcp has `RunningService::peer_info()` returning the server `InitializeResult` (fixture `initialize_peer_info`).
- rmcp adds `_meta.progressToken` to every request. Both servers accept it.
- claude-context reads and rewrites the **global** file `~/.context/mcp-codebase-snapshot.json` at every
  start (and uses `~/.context/merkle/`). Two instances race on it, and it lists every codebase the user indexed
  anywhere. A test that indexes into it changes the user's state.

## 2. Tools (raw lists: `tools_list.json`)

Semble (output schema for both: `{"result": string}`):

- `search(query: string, repo: string | string[], top_k: int >= 1 = 5, max_snippet_lines: int >= 0 | null = 10, content: "code"|"docs"|"config"|"all"|null = null)`
- `find_related(file_path: string, line: int (1-based), repo: string | string[], top_k = 5, max_snippet_lines = 10, content = null)`

claude-context (no output schema; all results are one text block):

- `index_codebase(path, force=false, splitter="ast"|"langchain"="ast", customExtensions=[], ignorePatterns=[])`
- `search_code(path, query, limit=10 (max 50), extensionFilter=[])`
- `clear_index(path)`
- `get_indexing_status(path)`

All `path` arguments must be absolute (claude-context says so; it resolves a relative one itself, see source `ensureAbsolutePath`).

## 3. Semble `search` output

Fixtures: `search_{retry_backoff,backend_url,hostselector}_{one,multi}` plus variants (`_top2`, `_snippet0`,
`_snippet_full`, `_top100`, `_workspace_root`, `_content_all`, `search_content_docs_workspace_root`,
`search_gibberish`, `find_related_one`).

- **Transport shape.** `content: [{type:"text", text:"<json>"}]` and `structuredContent: {"result": "<same json string>"}`.
  The JSON is a *string inside* the text, so it needs a second `serde_json::from_str`. Use `structuredContent.result`
  or `content[0].text` (identical), whichever the adapter prefers.
- **Inner shape (one repo):**
  `{"query": "...", "results": [{"file_path": "core/src/main/kotlin/sample/core/RetryPolicy.kt", "start_line": 1, "end_line": 18, "score": 0.0245, "content": "..."}]}`
- **Paths** are relative to the repo root for a single repo. With `repo` = list of paths, `file_path` is
  prefixed with a repo label (the directory name, e.g. `sample-web/src/http/fetchWithRetry.ts`) and the JSON gets
  `"repos": {"sample-android": "$WORKSPACE/sample-android", ...}` mapping label to absolute path.
  Passing the non-git workspace root as `repo` also works and gives root-relative paths (`sample-android/core/...`)
  with no `repos` map.
- **Lines** are 1-based and `end_line` is the end of the whole chunk. With `max_snippet_lines=10` the snippet holds
  only the first 10 lines of the chunk, so the snippet covers `start_line .. start_line+9`, not up to `end_line`.
- **Score** for `search` is a rank-fusion value (hybrid BM25 + embeddings), observed range 0.004-0.103. It is
  only good for ordering inside one response; do not compare across queries, repos or with `find_related`
  (`find_related` scores look like cosine similarity, 0.14-0.40). Exact symbol queries score much higher
  (`HostSelector`: 0.103 vs 0.0245 for the natural-language query).
- **Results count.** `top_k` (default 5, minimum 1, no maximum). `top_k=100` returned 13 results (all chunks of the
  project). `top_k=0` is rejected (below). Snippet control: `max_snippet_lines` 0 drops `content` entirely, `null`
  gives the full chunk.
- **`content`** defaults to `code`. README.md is only found with `content="docs"` or `"all"`.
- **Semantic search never returns empty on a non-empty index**: a nonsense query still returns 5 results
  (`search_gibberish`). The source returns `{"error": "No results found."}` (plain, `isError:false`) when it has none.
- `find_related(file_path, line)`: `file_path` is the repo-relative path as returned by `search`. The chunk
  containing the line is excluded from the output. Result shape is the same JSON, with
  `"query": "Chunks related to <file>:<line>"`.

## 4. Errors

Semble (fixtures `search_missing_path`, `search_empty_folder`, `search_file_path`, `search_relative_path_missing`,
`search_multi_one_missing`, `search_top_k_zero`, `search_missing_param`, `find_related_missing_file`, `unknown_tool`):

| Case | `isError` | Text |
|---|---|---|
| missing path | **false** | `Failed to index '<repo>': Path does not exist: <abs>` |
| empty folder | **false** | `Failed to index '<repo>': No supported files found under <abs>.` |
| repo is a file | **false** | `Failed to index '<file>': Path is not a directory: <file>` |
| list with one bad path | **false** | same `Failed to index ...` for the bad one, no partial results |
| relative path | n/a | resolved against the **server's cwd** (`search_relative_path_ok` succeeds when cwd contains it) |
| `top_k=0` / missing `repo` | true | `Error executing tool search: 1 validation error for searchArguments ...` (pydantic) |
| unknown tool | true | `Unknown tool: no_such_tool` |
| `find_related` on unknown file | **false** | `No chunk found at <file>:<line>. Make sure the file is indexed and ...` |

Adapter rule: a Semble result whose text does not start with `{` is an error even when `isError` is false; also
treat a JSON object with `"error"` and no `"results"` as "no results". Always pass absolute paths.
Semble errors never come as JSON-RPC errors; rmcp `call_tool` returned `Ok(CallToolResult)` in all cases.

claude-context with **Milvus down** (fixtures `milvus_down_*`, `milvus_hung_*`):

- `initialize` and `tools/list` succeed (about 2 ms for `tools/list`) because the Milvus connection attempt runs in
  the background and does not gate the MCP handshake.
- About 150 ms after the process starts, the Milvus client's connect rejects with
  `Error: 14 UNAVAILABLE: No connection established ... ECONNREFUSED 127.0.0.1:19530` and nothing catches the
  rejection (`node:internal/process/promises ... triggerUncaughtException`). **The node process exits (code 1).**
  A call in flight gets no response; rmcp returns `ServiceError::TransportClosed` ("Transport closed"). Every
  later call returns the same error immediately. There is no `isError` result at all.
- E1's "`tools/list` not answered within 4.5 s" is explained by this: its client sent `tools/list` after 1.5 s,
  by which time the server had already died. Sent at once (as rmcp does), `tools/list` is answered.
- With Milvus **hung** (TCP accepts, never answers gRPC; simulated with a local socket) the server stays alive and
  answers `tools/list`, but every tool call blocks (each `search_code` first calls `syncIndexedCodebasesFromCloud`,
  which hits Milvus). The Milvus client deadline of 15 s expires and the same uncaught rejection kills the process
  at about 15.7 s after start; the call in flight gets `TransportClosed` at that moment.
- `rmcp::peer().is_transport_closed()` turns true only after the next send fails; `RunningService::is_closed()`
  stayed false for a dead child (4 s observed). Detect a dead server from `ServiceError::TransportClosed`.

claude-context tool-level errors, **from source** (not observed live because Milvus was down); all `isError:true`
unless noted: `Error: Path '<abs>' does not exist. Original input: '<in>'`, `Error: Path '<abs>' is not a directory`,
`Error: Codebase '<abs>' is not indexed. Please index it first using the index_codebase tool.`,
`Error: Invalid file extensions in extensionFilter: [...]`, `Error: Index data for '<path>' has been lost (collection not found in Milvus). Please re-index using index_codebase with force=true.`,
`Error searching code: <msg> Please check if the codebase has been indexed first.`; non-error text:
`No results found for query: "<q>" in codebase '<path>'` and the collection-limit message.
`search_code` success format (source): one text block,
`Found N results for query: "<q>" in codebase '<root>'\n\n1. Code snippet (<lang>) [<root basename>]\n   Location: <relpath>:<start>-<end>\n   Rank: 1\n   Context: \n```<lang>\n<code>\n```\n\n2. ...`
(relative paths, no score, snippet truncated at 5000 chars, hard cap `limit` 50). Re-record this once Milvus is up.

## 5. Concurrency (one connection, `peer.call_tool` from 4 tasks)

Semble, search over a 4 MB / 176-file Rust crate:

| scenario | sequential | concurrent (4 calls) |
|---|---|---|
| warm, 4 different queries (3 rounds) | 15, 15, 19 ms total (3-6 ms each) | 10, 11, 13 ms total (each 5-13 ms, completion times staggered) |
| cold, 4 different repos (copies of the same crate) | 3787 ms total (first 1849 ms includes the model load; the others 634-658 ms) | 2595 ms total (all four finish at 2.5-2.6 s) |

Reading: the server accepts concurrent requests on one connection. Warm searches are CPU-bound synchronous code in
the asyncio loop, so they serialize (staggered completions) but each takes only a few ms. Cold index builds run in
threads (`asyncio.to_thread`) and overlap: roughly 1.9x faster than sequential once the model is loaded. Concurrent
requests for the *same* cold repo share one build (`asyncio.shield`ed task). claude-context concurrency could not be
measured (Milvus down).

## 6. Lifecycle

Spawn with a process group via `process-wrap` (already a dependency of rmcp; add it explicitly to use the types):

```rust
let mut cmd = CommandWrap::with_new("semble", |c| { c.env("X", "y"); });
cmd.wrap(ProcessGroup::leader());   // setpgid(0,0): server and its children get their own group, pgid == child pid
cmd.wrap(KillOnDrop);
```

Process trees seen (`ps -axo pid,ppid,pgid`):

- Semble: `semble` (the uv-tool venv `python`) plus one `python -B -c "from multiprocessing.resource_tracker import main;main(9)"`
  child. Both in the group of the root pid. No other children observed on a 4 MB repo.
- claude-context: a single `node .../dist/index.js` process, no children.

Shutdown behaviour (time from the event until the server pid is gone, own-process-group launch):

| event | Semble | claude-context |
|---|---|---|
| `drop(client)` with runtime alive | 0.01 s | **3.05 s** (rmcp's graceful path: close stdin, wait 3 s, SIGTERM) |
| `client.cancel().await` | returns in 4 ms, gone | returns after **3015 ms**, gone |
| `drop(client)` then immediate `process::exit` | 0.05 s | **orphaned for 3.5 s** (see below) |
| parent `kill -9` | 0.06 s | **orphaned for 2.4 s** |

Why: Semble exits on stdin EOF. **claude-context ignores stdin EOF** (verified: alive 10 s after EOF with stderr
still open). rmcp 3.9's `graceful_shutdown` waits 3 s for exit, then SIGTERM, then kill. If the parent dies first, the
kill never happens; the orphan survives until its next write to the now-closed stderr pipe raises EPIPE
(verified: closing the read end of stderr makes it exit with code 1 exactly at the 5 s initial-sync log line).
After that first sync the next stderr write is up to 5 minutes away, so a parent that dies later than 5 s after
spawn can leave a claude-context orphan for up to ~5 min (the 5 min part was inferred from the source's sync
period, not waited out).

Recommendations:

1. Own process group (above) and record `pgid` (= child pid).
2. Do not rely on rmcp's drop/`cancel()` for claude-context. On shutdown: `killpg(pgid, SIGTERM)`, short wait
   (200 ms), `killpg(pgid, SIGKILL)`, then drop the client. This makes claude-context shutdown instant.
3. Write a pidfile per child and reap stale ones (`ppid == 1`, command line contains
   `claude-context-mcp/dist/index.js`) at orchestrator start, to cover `kill -9` of the orchestrator.
4. Pipe stderr and always drain it (a task that reads lines into a ring buffer / log file). rmcp's builder default
   is `Stdio::inherit()`; claude-context logs ~55 lines at start, so inheriting pollutes the orchestrator's
   stderr, and an undrained pipe would block the server at 64 KB.
5. At the end of every test run assert `pgrep -fl semble` and `pgrep -fl claude-context-mcp` are empty.

Not done: `semble --http --port N` (streamable HTTP, would bind a port and keep the model resident). Not built on.

## 7. Cold start

| step | Semble | claude-context |
|---|---|---|
| spawn to initialize done | 0.92 / 1.06 / 1.48 s (2.08 s first run) | 0.45-1.0 s |
| first search, tiny fixture (3 small projects), model already loaded | 272-411 ms | n/a |
| first search at once after init on a 4 MB crate (waits for model prewarm + builds index) | 1069 ms (1849 ms in the sequential-cold series) | n/a |
| first search 3 s after init, 4 MB / 176 files | 716 ms | n/a |
| first search, index already on disk (`~/Library/Caches/semble/<hash>/`), new process | 314 ms | n/a |
| warm search (same process) | 1-30 ms (`find_related` 26 ms) | n/a |

Semble loads its embedding model in a background task started at process launch; the first search awaits it, so
the first search right after `initialize` pays up to ~1 s more. Index build is roughly 0.15-0.2 s per MB here.
The cache is keyed by path: a fresh copy of a directory is cold again.

## 8. Cancellation

rmcp 3.9.0: dropping the `call_tool` future (what `tokio::time::timeout` does) **does** send
`notifications/cancelled`. Wire log:

```
 893ms C->S {"jsonrpc":"2.0","id":1,"method":"tools/call", ... "name":"search"}
1045ms C->S {"jsonrpc":"2.0","method":"notifications/cancelled","params":{"reason":"request abandoned by the caller","requestId":1}}
1045ms C->S {"jsonrpc":"2.0","id":2,"method":"tools/call", ...}
```

(timeout of 150 ms around a cold search.) The server sent no response for id 1 (it honours the cancel). The
connection stays usable: the follow-up call returned the full result. For Semble the cold index build is a shielded
background task, so it is *not* aborted: the follow-up call (830 ms) simply waited for the same build, and a third
call took 3 ms. Dropping at the deadline therefore wastes nothing for Semble; a retry after the deadline is cheap.
claude-context cancellation was not testable (the server is dead without Milvus).
Alternative if an explicit reason is wanted: rmcp's own `PeerRequestOptions` timeout sends reason `"request timeout"`.

## Working snippet

Real code from the spike (`uals-spikes/s1-rmcp`, outside the repo), trimmed:

```rust
use std::{process::Stdio, time::Duration};
use process_wrap::tokio::{CommandWrap, KillOnDrop, ProcessGroup};
use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};

// Cargo: rmcp = { version = "=3.9.0", features = ["client", "transport-child-process"] }
//        process-wrap = { version = "10.0", features = ["tokio1"] }, tokio, serde_json, libc

let mut cmd = CommandWrap::with_new("/path/to/semble", |_c| {});
cmd.wrap(ProcessGroup::leader());                     // own group, pgid == child pid
cmd.wrap(KillOnDrop);

let (transport, stderr) = TokioChildProcess::builder(cmd).stderr(Stdio::piped()).spawn()?;
let pgid = transport.id().expect("pid") as i32;
tokio::spawn(drain(stderr.expect("piped")));           // always drain stderr

let client = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
    .await??;                                          // initialize + notifications/initialized
// client.peer_info() -> server InitializeResult (protocol version, serverInfo, capabilities)

let mut p = CallToolRequestParams::new("search");
p.arguments = serde_json::json!({"query": "HostSelector", "repo": "/abs/path"}).as_object().cloned();
let res = tokio::time::timeout(Duration::from_secs(3), client.call_tool(p)).await;
//   Err(_)       -> deadline: the future was dropped, rmcp sent notifications/cancelled, connection still usable
//   Ok(Err(e))   -> ServiceError (TransportClosed = server died: restart)
//   Ok(Ok(r))    -> CallToolResult { content, structured_content, is_error, .. }

// shutdown: do not wait for rmcp's 3 s graceful path (claude-context ignores stdin EOF)
unsafe { libc::killpg(pgid, libc::SIGTERM); }
tokio::time::sleep(Duration::from_millis(200)).await;
unsafe { libc::killpg(pgid, libc::SIGKILL); }
drop(client);
```

## Recommended deadlines

| | Semble | claude-context |
|---|---|---|
| spawn + initialize | 10 s | 10 s (and probe Milvus first, see below) |
| warm query | 3 s (typical 5-30 ms) | 10 s (not measured; embedding call to LM Studio plus Milvus query) |
| first query on a repo, cold | 30 s for repos up to ~100 MB at ~0.2 s/MB; on timeout keep waiting by retrying (build continues) | `index_codebase` returns quickly and indexes in the background; poll `get_indexing_status` (not measured) |
| shutdown | 1 s (EOF is enough, then killpg) | 0.2 s then SIGKILL the group |

The orchestrator's per-search budget can stay at the global deadline; a cold Semble repo only needs a larger
deadline for the first call per path (or a prewarm call at startup).

## Restart strategy for a crashed server

- Detect: `ServiceError::TransportClosed` from `call_tool`/`list_tools`, or child exit (`transport.id()` pid gone).
  Do not trust `is_closed()`.
- Semble: respawn on demand at the next request (1-1.5 s init), indexes are on disk, so the next search is
  ~0.3 s. Exponential backoff after 3 consecutive failures (1 s, 2 s, 4 s ... cap 60 s). Do not replay the failed
  call automatically more than once.
- claude-context: before every (re)spawn, check that Milvus answers (`curl --noproxy '*' http://127.0.0.1:9091/healthz`,
  or a TCP connect to 19530). If not, do not spawn: the process would die within 150 ms. Mark the backend
  unavailable (degraded result, not an error for the whole query) and retry the probe with backoff (5 s to 60 s).
  After Milvus is back, respawn, `get_indexing_status`, and treat "not indexed" as a user-visible state, not a crash.
- Never run two claude-context instances against the same `~/.context` at once (snapshot file race).
- Kill the whole group (`killpg`) before respawning, never only the child pid.

## Gotchas

1. Semble's inner JSON is a string inside a string field: parse twice.
2. Semble returns failures as `isError:false` text. Detect by "does not start with `{`".
3. Semble `end_line` is the chunk end, not the snippet end; relative `repo` resolves against the server cwd; use absolute paths.
4. Semble scores are rank-fusion values: use rank only.
5. Multi-repo: file paths are prefixed by the repo directory name; map back through `repos`. Two repos with the same
   directory name may be disambiguated (not tested).
6. claude-context dies when Milvus is down at startup (or unreachable for 15 s) instead of reporting an error. Always
   probe Milvus first.
7. claude-context ignores stdin EOF: needs explicit group kill; orphans linger until an EPIPE on stderr.
8. rmcp `cancel()` and drop take 3 s for such servers; `RunningService::is_closed()` does not notice a dead child.
9. rmcp 3.9.0 sends `protocolVersion: 2026-07-28`; both servers downgrade to `2025-11-25`. Pin nothing; just accept.
10. rmcp's `TokioChildProcess::builder` defaults stderr to `inherit`; pipe it and drain it.
11. claude-context uses global state in `~/.context/` and cwd-dependent logging (`Current workspace: <cwd>`).
12. Semble cache `~/Library/Caches/semble/<hash>/` is created per path, per content selection (`index-code-config-docs`
    variants exist); tests on throw-away copies leave cache directories behind, clean only your own.
13. rmcp adds `_meta.progressToken` to requests; harmless for both.

## Fixture conventions

- Fixture root path is replaced by `$WORKSPACE` and the home directory by `$HOME`. No absolute user paths are committed.
- Semble `<name>.json`: serialized `CallToolResult` (`content`, `isError`, `structuredContent`). `tools_list.json` is
  `{"tools": [...]}`. `initialize_peer_info.json` is the server `InitializeResult`.
- claude-context `milvus_*` fixtures are **not** tool results: they record what rmcp returned when the server died:
  `{"service_error": "Transport closed", "debug": "TransportClosed"}` (or `{"client_timeout_ms": N}`); the meta `notes`
  carry the timing and crash cause. The adapter test should map these to "backend unavailable".
- `duration_ms` in the meta files is wall time of that call on the spike machine and is informational.
