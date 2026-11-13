---
name: uals
description: Local code search for AI coding agents across every repository in a folder. Use it to find code by symbol name, plain words, regex or exact text, and to find every usage (all references) of a function or class with IDE precision through language servers (Kotlin, Java, Go, Rust, TypeScript, Python, C/C++, Swift). Installs and uses the uals MCP server: one ranked answer from ast-index symbols, ripgrep text search and semantic search, instead of many grep and file reads.
---

# uals: code search for agents

uals is a local MCP server (`uals mcp`) and a command line tool. It searches every repository of a folder at once and returns one ranked list with code snippets. Nothing leaves the machine.

## Install

Check first: `uals doctor`. If `uals` is not on `PATH`, install it (macOS or Linux):

```bash
git clone https://github.com/zhimbura/ultimate-ai-local-search.git
cd ultimate-ai-local-search
./install.sh
```

The installer puts `uals` and `ast-index` into `~/.local/bin` and registers the MCP server in Claude Code. Restart the agent after it. Other MCP clients: run `uals mcp` as a stdio server.

For precise usages, install the language server of each language in the project, for example `gopls`, `rust-analyzer`, `typescript-language-server`, `pyright` or `kotlin-lsp`. `uals doctor` lists the installed ones.

## Use

- `search` (`mcp__uals__search`) first for any code question: a symbol name, plain words, `/regex/` or `"exact text"`. One call covers every repository.
- Plain words are searched by meaning and symbol names. For exact text, quote it or write a `/regex/`.
- `usages` (`mcp__uals__usages`) lists every reference to a symbol. `Container.name` (for example `Store.load`) keeps only the usages of that declaration. `[n]` after a line number names the declaration the usage refers to.
- `status` (`mcp__uals__status`) shows the index state. When results look stale, run `uals refresh`.

Command line: `uals search "<query>"`, `uals usages <symbol>`, `uals status`, `uals refresh`.
