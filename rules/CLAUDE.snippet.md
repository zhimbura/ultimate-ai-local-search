## Code search (uals)

- `mcp__uals__search` first for any code question: a code name, plain words, `/regex/` or `"exact text"`. It covers every repository of the workspace.
- `mcp__uals__usages` for every reference to a symbol.
- `mcp__uals__status` when results look stale, then run `uals refresh`.
- ast-index CLI for structure: `hierarchy`, `implementations`, `callers`, `call-tree`, `deps`, `dependents`.
- No grep fallback: `search` already covers text and respects `.gitignore`.
