#!/usr/bin/env bash
# Undo what install.sh recorded in its manifest (~/.local/share/uals/install-manifest).
#   ./uninstall.sh            show the list, ask once, undo it in reverse order
#   ./uninstall.sh --yes      do not ask
#   ./uninstall.sh --purge    also delete the Milvus volume (indexed vectors), only when the
#                             installer started the stack. Irreversible.
# Without a manifest: removes the uals MCP entry if present and says what is left.
# Safe to run twice.
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ -t 1 ]; then G=$'\033[32m'; Y=$'\033[33m'; R=$'\033[31m'; C=$'\033[36m'; N=$'\033[0m'; else G=; Y=; R=; C=; N=; fi
say()  { printf '%s\n' "${C}▸${N} $*"; }
ok()   { printf '%s\n' "${G}✓${N} $*"; }
warn() { printf '%s\n' "${Y}!${N} $*" >&2; }
die()  { printf '%s\n' "${R}✗ $*${N}" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

ASSUME_YES=0; PURGE=0
for arg in "$@"; do case "$arg" in
  --yes|-y) ASSUME_YES=1;;
  --purge) PURGE=1;;
  -h|--help) sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0;;
  *) die "unknown flag: $arg";;
esac; done

DATA_DIR="$HOME/.local/share/uals"
MANIFEST="$DATA_DIR/install-manifest"
CLAUDE_CONFIG="${CLAUDE_CONFIG:-$HOME/.claude.json}"

# Only an absolute path other than / and the home folder may be deleted or moved.
safe_path() { case "$1" in ''|/|"$HOME"|"$HOME"/) return 1;; /*) return 0;; *) return 1;; esac; }

if [ ! -f "$MANIFEST" ]; then
  say "no install manifest at $MANIFEST"
  if [ -f "$CLAUDE_CONFIG" ] && have jq && jq -e '.mcpServers.uals' "$CLAUDE_CONFIG" >/dev/null 2>&1; then
    bash "$REPO_DIR/scripts/configure-mcp.sh" --remove --config "$CLAUDE_CONFIG"
  else
    say "no uals MCP entry in $CLAUDE_CONFIG"
  fi
  echo "Left in place: the uals binary and its tools in your bin dir, the uals config"
  echo "(\${XDG_CONFIG_HOME:-~/.config}/uals), the Milvus stack and ${DATA_DIR}. Remove them yourself if you want."
  exit 0
fi

MANIFEST_LINES=()
while IFS= read -r line || [ -n "$line" ]; do
  [ -z "$line" ] || MANIFEST_LINES+=("$line")
done < "$MANIFEST"

if [ "${#MANIFEST_LINES[@]}" = 0 ]; then rm -f -- "$MANIFEST"; ok "the manifest was empty: removed it"; exit 0; fi

echo "uninstall.sh will undo, newest first:"
for ((i = ${#MANIFEST_LINES[@]} - 1; i >= 0; i--)); do echo "  ${MANIFEST_LINES[$i]}"; done
case "$PURGE:$(printf '%s\n' "${MANIFEST_LINES[@]}" | grep -c '^docker-stack ' || true)" in
  1:0) warn "--purge: the installer did not start the Milvus stack, so no volume is deleted";;
  1:*) warn "--purge: the Milvus volume (indexed vectors) will be deleted";;
esac
if [ "$ASSUME_YES" = 0 ]; then
  read -rp "Proceed? [y/N] " answer
  case "$answer" in y|Y|yes) ;; *) echo "Nothing changed."; exit 0;; esac
fi

# find_uv: the uv the installer put in place, else the one on PATH
find_uv() {
  local line uv_path
  for line in "${MANIFEST_LINES[@]}"; do
    case "$line" in 'uv-installed '*)
      uv_path="${line#uv-installed }"
      [ ! -x "$uv_path" ] || { printf '%s' "$uv_path"; return; };;
    esac
  done
  command -v uv || true
}

for ((i = ${#MANIFEST_LINES[@]} - 1; i >= 0; i--)); do
  kind="${MANIFEST_LINES[$i]%% *}"; value="${MANIFEST_LINES[$i]#* }"
  case "$kind" in
    file|config-file)
      safe_path "$value" || { warn "skipping unsafe path: $value"; continue; }
      if [ -e "$value" ]; then rm -f -- "$value"; ok "removed $value"; fi
      [ "$kind" != config-file ] || rmdir "$(dirname "$value")" 2>/dev/null || true
      ;;
    dir)
      safe_path "$value" || { warn "skipping unsafe path: $value"; continue; }
      if [ -d "$value" ]; then rm -rf -- "$value"; ok "removed $value"; fi
      ;;
    emptydir)
      rmdir "$value" 2>/dev/null || true
      ;;
    moved-aside)
      # A file that was in place before the install: put it back where it was.
      original="${value%.bak.*}"
      safe_path "$original" || { warn "skipping unsafe path: $value"; continue; }
      if [ -e "$value" ] && [ ! -e "$original" ]; then mv -f -- "$value" "$original"; ok "restored $original"; fi
      ;;
    mcp-entry)
      bash "$REPO_DIR/scripts/configure-mcp.sh" --remove --config "$value"
      ;;
    saved-mcp)
      bash "$REPO_DIR/scripts/configure-mcp.sh" --restore-claude-context --config "$value"
      ;;
    uv-tool)
      uv_bin="$(find_uv)"
      if [ -n "$uv_bin" ]; then
        UV_TOOL_BIN_DIR="$value" "$uv_bin" tool uninstall semble >/dev/null 2>&1 && ok "removed the semble tool" || true
      else warn "uv not found: run 'uv tool uninstall semble' yourself"; fi
      ;;
    uv-installed)
      # uv's own uninstall steps (docs.astral.sh/uv/getting-started/installation).
      if [ -x "$value" ]; then
        "$value" cache clean >/dev/null 2>&1 || true
        for d in "$("$value" python dir 2>/dev/null || true)" "$("$value" tool dir 2>/dev/null || true)"; do
          if safe_path "$d" && [ -d "$d" ]; then rm -rf -- "$d"; rmdir "$(dirname "$d")" 2>/dev/null || true; fi
        done
        rm -f -- "$value"; ok "removed uv ($value) with its Python and tool folders"
      fi
      ;;
    docker-stack)
      if [ ! -f "$value/docker-compose.yml" ]; then warn "no docker-compose.yml in $value: stop the uals-milvus container yourself"; continue; fi
      if ! have docker || ! docker info >/dev/null 2>&1; then warn "Docker is not running: stop the uals-milvus stack yourself"; continue; fi
      env_args=(); [ ! -f "$value/.env" ] || env_args=(--env-file "$value/.env")
      if [ "$PURGE" = 1 ]; then
        say "stopping the Milvus stack and deleting its volume…"
        (cd "$value" && docker compose ${env_args[@]+"${env_args[@]}"} down --volumes) || warn "docker compose down failed"
      else
        say "stopping the Milvus stack (volume kept; --purge deletes it)…"
        (cd "$value" && docker compose ${env_args[@]+"${env_args[@]}"} down) || warn "docker compose down failed"
      fi
      ;;
    *) warn "unknown manifest line: ${MANIFEST_LINES[$i]}";;
  esac
done

rm -f -- "$MANIFEST"
rmdir "$DATA_DIR/removed-mcp" 2>/dev/null || true
rmdir "$DATA_DIR" 2>/dev/null || true
ok "install manifest removed"
echo "Done. Not touched: Ollama, LM Studio, Docker, and anything that was on your machine before the install."
