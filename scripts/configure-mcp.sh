#!/usr/bin/env bash
# Safely merge the claude-context MCP server block into an agent config (default ~/.claude.json).
# Only touches the "claude-context" key — your other MCP servers and secrets are preserved.
# Always makes a timestamped backup first (keeps the 3 newest this script created).
# A symlinked config is written THROUGH the link, never replaced.
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/env.sh
. "$REPO_DIR/scripts/lib/env.sh"

ENV_FILE=""; CONFIG="${CLAUDE_CONFIG:-$HOME/.claude.json}"
while [ $# -gt 0 ]; do case "$1" in
  --env-file) ENV_FILE="$2"; shift 2;;
  --config)   CONFIG="$2"; shift 2;;
  *) shift;;
esac; done

[ -n "$ENV_FILE" ] && [ -f "$ENV_FILE" ] || { echo "✗ env file not found: $ENV_FILE" >&2; exit 1; }
command -v jq >/dev/null || { echo "✗ jq required" >&2; exit 1; }

env_load_file "$ENV_FILE"
CC_VERSION="${CLAUDE_CONTEXT_VERSION:-0.1.15}"

# Resolve symlinks (readlink -f is not portable to macOS) so we edit the real file.
resolve_link() {
  local p="$1" l
  while [ -L "$p" ]; do
    l="$(readlink "$p")"
    case "$l" in /*) p="$l";; *) p="$(dirname "$p")/$l";; esac
  done
  printf '%s' "$p"
}
CONFIG="$(resolve_link "$CONFIG")"

mkdir -p "$(dirname "$CONFIG")"
[ -f "$CONFIG" ] || echo '{}' > "$CONFIG"
jq -e . "$CONFIG" >/dev/null 2>&1 || { echo "✗ $CONFIG is not valid JSON — aborting (won't risk your config)" >&2; exit 1; }

# Build env object — include only non-empty keys claude-context actually reads.
env_obj=$(jq -n \
  --arg MILVUS_ADDRESS     "${MILVUS_ADDRESS:-}" \
  --arg EMBEDDING_PROVIDER "${EMBEDDING_PROVIDER:-}" \
  --arg EMBEDDING_MODEL    "${EMBEDDING_MODEL:-}" \
  --arg EMBEDDING_DIMENSION "${EMBEDDING_DIMENSION:-}" \
  --arg OPENAI_API_KEY     "${OPENAI_API_KEY:-}" \
  --arg OPENAI_BASE_URL    "${OPENAI_BASE_URL:-}" \
  --arg VOYAGEAI_API_KEY   "${VOYAGEAI_API_KEY:-}" \
  --arg GEMINI_API_KEY     "${GEMINI_API_KEY:-}" \
  --arg OPENROUTER_API_KEY "${OPENROUTER_API_KEY:-}" \
  --arg OLLAMA_HOST        "${OLLAMA_HOST:-}" \
  '{MILVUS_ADDRESS:$MILVUS_ADDRESS, EMBEDDING_PROVIDER:$EMBEDDING_PROVIDER, EMBEDDING_MODEL:$EMBEDDING_MODEL}
   | (if $EMBEDDING_DIMENSION!="" then .EMBEDDING_DIMENSION=$EMBEDDING_DIMENSION else . end)
   | (if $OPENAI_API_KEY!=""     then .OPENAI_API_KEY=$OPENAI_API_KEY else . end)
   | (if $OPENAI_BASE_URL!=""    then .OPENAI_BASE_URL=$OPENAI_BASE_URL else . end)
   | (if $VOYAGEAI_API_KEY!=""   then .VOYAGEAI_API_KEY=$VOYAGEAI_API_KEY else . end)
   | (if $GEMINI_API_KEY!=""     then .GEMINI_API_KEY=$GEMINI_API_KEY else . end)
   | (if $OPENROUTER_API_KEY!="" then .OPENROUTER_API_KEY=$OPENROUTER_API_KEY else . end)
   | (if $OLLAMA_HOST!=""        then .OLLAMA_HOST=$OLLAMA_HOST else . end)')

backup="${CONFIG}.bak.$(date +%Y%m%d-%H%M%S)"
cp -p "$CONFIG" "$backup"

# Rotate: keep the 3 newest backups of the exact form <config>.bak.YYYYmmdd-HHMMSS.
# Anything else (e.g. .bak.20260619-claude-context-fix) is never touched.
old_backups=()
for f in "${CONFIG}".bak.*; do
  [ -e "$f" ] || continue
  case "${f##*.bak.}" in
    [0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[0-9][0-9][0-9][0-9][0-9][0-9]) old_backups+=("$f");;
  esac
done
if [ "${#old_backups[@]}" -gt 3 ]; then
  # fixed-width timestamps sort lexically == chronologically
  while IFS= read -r f; do rm -f -- "$f"; done < <(printf '%s\n' "${old_backups[@]}" | sort | head -n "$(( ${#old_backups[@]} - 3 ))")
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
jq --argjson env "$env_obj" --arg ccv "$CC_VERSION" '
  .mcpServers = (.mcpServers // {})
  | .mcpServers."claude-context" = {
      type: "stdio",
      command: "npx",
      args: ["-y", "@zilliz/claude-context-mcp@" + $ccv],
      env: $env
    }
' "$CONFIG" > "$tmp"
[ -s "$tmp" ] || { echo "✗ jq produced empty output — $CONFIG left untouched" >&2; exit 1; }
cat "$tmp" > "$CONFIG"   # write in place: keeps symlink, inode and permissions

echo "✓ claude-context MCP written to $CONFIG (backup: $backup)"
echo "  restart your agent to load the new config"
