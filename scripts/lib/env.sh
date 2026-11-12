#!/usr/bin/env bash
# Safe .env parser — sourced by install.sh and scripts/*.sh. Never executes file contents.
#   env_load_file <file> [override|defaults]
#     override (default): every valid KEY=VALUE line is exported, replacing the current value.
#     defaults:           a key is exported only if it is unset/empty in the current environment.
# Accepted lines: KEY=VALUE, optional `export ` prefix, "double" or 'single' quoted values,
# `# comments`, blank lines. Unquoted values lose trailing whitespace and ` # inline comment`.
# Only the FIRST '=' splits key from value, so values may contain '='. Nothing is expanded
# ($VAR, backticks and $(...) stay literal text). Invalid lines are ignored.

env_load_file() {
  local file="$1" mode="${2:-override}" line key val
  [ -f "$file" ] || return 1
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    line="${line#"${line%%[![:space:]]*}"}"                 # ltrim
    case "$line" in ''|'#'*) continue;; esac
    case "$line" in 'export '*) line="${line#export }"; line="${line#"${line%%[![:space:]]*}"}";; esac
    case "$line" in *=*) ;; *) continue;; esac
    key="${line%%=*}"; val="${line#*=}"
    key="${key%"${key##*[![:space:]]}"}"                    # rtrim key
    case "$key" in ''|[0-9]*|*[!A-Za-z0-9_]*) continue;; esac
    val="${val#"${val%%[![:space:]]*}"}"                    # ltrim value
    case "$val" in
      \"*) val="${val#\"}"; val="${val%%\"*}";;             # up to the closing quote
      \'*) val="${val#\'}"; val="${val%%\'*}";;
      *)   case "$val" in *' #'*|*$'\t#'*) val="${val%%[[:space:]]#*}";; esac
           val="${val%"${val##*[![:space:]]}"}";;
    esac
    if [ "$mode" = defaults ] && [ -n "${!key:-}" ]; then continue; fi
    printf -v "$key" '%s' "$val"
    export "${key?}"                                         # exports the variable NAMED by $key
  done < "$file"
}
