<!-- Compact version for CLAUDE.md. Full version: rules/AGENTS.snippet.md -->

## Code search policy (ast → vector → grep)

Pick the tool by query type, not habit:

1. **ast-index** (default) — symbols & structure: classes, functions, usages, implementations, call-tree, hierarchy. Fast, exact. Session start: `ast-index stats` (rebuild/update if stale).
2. **vector / claude-context** (`mcp__claude-context__search_code`) — conceptual "find by meaning" queries when you don't know the symbol name. Only where the project is indexed (`get_indexing_status` = `completed`), else skip.
3. **rg/grep** — LAST RESORT, with the user's OK. **No approval needed** where ast/vector can't help anyway: regex, exact strings, error text, non-code files (JSON/YAML/MD/SQL/.env), freshly edited code.

Don't grep "for completeness" after tier 1/2 succeed.
