## Code search (uals)

Use the uals MCP tools for code questions. Claude Code shows them as `mcp__uals__search`, `mcp__uals__usages` and `mcp__uals__status` when the server is registered as `uals`. Other agents see `search`, `usages` and `status`.

- `search`: use it first for any question about the code. The query can be a code name (`HostSelector`), plain words (`where is the retry logic`), a `/regex/` or an `"exact text"`. It searches every repository of the workspace at once: symbols, text and meaning.
- `usages`: use it for every reference to a symbol.
- `status`: use it when results look stale or a backend is missing. Then run `uals refresh` in a shell.
- Exact text: plain words are searched by meaning and symbol names, not as text, and so is one lowercase word (`retry`). To find a string or a pattern, quote it (`"connection refused"`), write a `/regex/`, or set `kind` to `literal` or `regex`. Those queries use the text search only. `search` covers every repository and respects `.gitignore`, so use it instead of grep or rg.

For structure, use the ast-index CLI, from inside the repository:

```
ast-index hierarchy <Class>            # class hierarchy
ast-index implementations <Parent>     # subclasses and implementors
ast-index callers <function>           # who calls a function
ast-index call-tree <function>         # call hierarchy, callers up
ast-index deps <module>                # module dependencies
ast-index dependents <module>          # modules that depend on this one
ast-index outline <file>               # symbols of a file
```

If a command prints nothing, the repository may have no index: run `uals refresh`.
