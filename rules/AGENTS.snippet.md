<!--
  Drop this section into your project's AGENTS.md (or CLAUDE.md).
  It tells coding agents how to pick a search tool. Tune the "ask first" line to taste.
-->

## Code search (ast → vector → grep)

Escalation by query type. Prefer the highest applicable tier; fall to plain grep only as a last resort and **with the user's OK** (see Tier 3).

### Tier 1 — ast-index (default: symbols & structure)

Structural AST index (tree-sitter → SQLite). Exact/prefix/contains on symbol names, far faster than grep. Best for symbols: classes, functions, usages, implementations, call-tree, hierarchy, project map. **Not** a by-meaning search.

```
ast-index search <q>            # quick find (add --fuzzy)
ast-index class|symbol <Name>   # definitions
ast-index usages|callers <Name> # references / call sites
ast-index implementations <Parent> · call-tree <Func> · hierarchy <Class>
ast-index outline <file> · map · conventions
```
At session start: `ast-index stats` → if missing, `ast-index rebuild`; if stale, `ast-index update`.

### Tier 2 — vector / semantic (claude-context MCP)

For "find by meaning" / conceptual queries where the symbol name is unknown — e.g. "where is retry logic handled", "how does auth work", "what rate-limits requests". Tool: `mcp__claude-context__search_code` (path = project root).

- Available **only where the project is indexed**: check `get_indexing_status`; if not indexed, status ≠ `completed`, or search errors → this tier is unavailable, move on.
- Strengths: fuzzy/conceptual lookups. Weaknesses: not for exact strings, regex, or non-code (those are Tier 3).

### Tier 3 — rg/grep (LAST RESORT — ask first)

Use only after Tier 1 and Tier 2 came up empty/unsuitable, and **only with the user's explicit OK**.

**Exception — no approval needed** where ast-index and vector physically can't help (they index symbols/meaning, not raw text):
- regex / exact string literals / error-message text
- non-code files: Markdown, JSON/YAML/TOML, shell scripts, SQL, HTML/CSS, .env, etc.
- freshly edited code not yet re-indexed
- cross-checking Tier 1/2 completeness when results look partial

> Don't grep "for completeness" after Tier 1/2 succeed — trust the index.
