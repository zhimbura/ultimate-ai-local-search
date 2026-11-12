# Test workspace

A public multi-repo fixture for `uals` tests. It is a parent folder with three independent projects:

| Folder | Stack | What it has |
|---|---|---|
| `sample-android` | Kotlin, Gradle (`:core`, `:network`, `:app`) | retry with backoff, backend host selection, config loading, analytics |
| `sample-web` | TypeScript | fetch with retry, session token refresh |
| `sample-tools` | Python | LRU cache, slugify |

Tests copy this folder to a temporary directory and run `git init` in each project, because a git repository cannot contain nested `.git` folders. Tests that check ignore rules create `build/` or `node_modules/` content at runtime.

Never put proprietary code, paths or queries here: this repository is public.
