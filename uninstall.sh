#!/usr/bin/env bash
# Undo what install.sh recorded in its manifest (~/.local/share/uals/install-manifest).
#   ./uninstall.sh            show the list, ask once, undo it in reverse order
#   ./uninstall.sh --yes      do not ask
#   ./uninstall.sh --purge    also delete the Milvus volume (indexed vectors), only when the
#                             installer started the stack. Irreversible.
# A step that fails is reported and skipped; its manifest line stays, so a re-run retries it.
# Without a manifest: removes the uals MCP entry if present; there is nothing else to undo.
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
  -h|--help) sed -n '2,9p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0;;
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
  echo "There is no manifest, so there is nothing to undo from it. If you installed uals by hand, remove it yourself:"
  echo "the uals and ast-index binaries, the uals config (\${XDG_CONFIG_HOME:-~/.config}/uals), the Milvus stack and ${DATA_DIR}."
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

# One function per manifest kind. They run inside an `if`, where `set -e` does not apply, so every
# command that can fail ends in `|| return 1`.
undo_file() {
  safe_path "$1" || { warn "skipping unsafe path: $1"; return 0; }
  if [ -e "$1" ]; then rm -f -- "$1" || return 1; ok "removed $1"; fi
}
undo_dir() {
  safe_path "$1" || { warn "skipping unsafe path: $1"; return 0; }
  if [ -d "$1" ]; then rm -rf -- "$1" || return 1; ok "removed $1"; fi
}
undo_emptydir() { rmdir "$1" 2>/dev/null || true; }
undo_moved_aside() {
  # A file that was in place before the install: put it back where it was.
  local original="${1%.bak.*}"
  safe_path "$original" || { warn "skipping unsafe path: $1"; return 0; }
  if [ -e "$1" ] && [ ! -e "$original" ]; then mv -f -- "$1" "$original" || return 1; ok "restored $original"; fi
}
undo_mcp_entry() { bash "$REPO_DIR/scripts/configure-mcp.sh" --remove --config "$1" || return 1; }
undo_saved_mcp() { bash "$REPO_DIR/scripts/configure-mcp.sh" --restore-claude-context --config "$1" || return 1; }
undo_docker_stack() {
  local env_args=()
  if [ ! -f "$1/docker-compose.yml" ]; then warn "no docker-compose.yml in $1: stop the uals-milvus container yourself"; return 0; fi
  if ! have docker || ! docker info >/dev/null 2>&1; then warn "Docker is not running: stop the uals-milvus stack yourself"; return 0; fi
  [ ! -f "$1/.env" ] || env_args=(--env-file "$1/.env")
  if [ "$PURGE" = 1 ]; then
    say "stopping the Milvus stack and deleting its volume…"
    (cd "$1" && docker compose ${env_args[@]+"${env_args[@]}"} down --volumes) || { warn "docker compose down failed"; return 1; }
  else
    say "stopping the Milvus stack (volume kept; --purge deletes it)…"
    (cd "$1" && docker compose ${env_args[@]+"${env_args[@]}"} down) || { warn "docker compose down failed"; return 1; }
  fi
}

failed=0
for ((i = ${#MANIFEST_LINES[@]} - 1; i >= 0; i--)); do
  line="${MANIFEST_LINES[$i]}"; kind="${line%% *}"; value="${line#* }"
  case "$kind" in
    file|config-file) step=undo_file;;
    dir)              step=undo_dir;;
    emptydir)         step=undo_emptydir;;
    moved-aside)      step=undo_moved_aside;;
    mcp-entry)        step=undo_mcp_entry;;
    saved-mcp)        step=undo_saved_mcp;;
    docker-stack)     step=undo_docker_stack;;
    *) warn "unknown manifest line: $line"; continue;;
  esac
  if ! "$step" "$value"; then
    warn "step failed, skipped: $line"
    step_failed[i]=1; failed=$((failed + 1))
  fi
done

if [ "$failed" -gt 0 ]; then
  # the failed lines go back into the manifest in their original order
  { for ((i = 0; i < ${#MANIFEST_LINES[@]}; i++)); do
      [ "${step_failed[$i]:-}" != 1 ] || printf '%s\n' "${MANIFEST_LINES[$i]}"
    done; } > "$MANIFEST.new"
  mv -f "$MANIFEST.new" "$MANIFEST"
  die "$failed step(s) failed: their lines are left in $MANIFEST. Fix the cause and run uninstall.sh again."
fi

rm -f -- "$MANIFEST"
rmdir "$DATA_DIR/removed-mcp" 2>/dev/null || true
rmdir "$DATA_DIR" 2>/dev/null || true
ok "install manifest removed"
echo "Done. Not touched: Ollama, LM Studio, Docker, and anything that was on your machine before the install."
