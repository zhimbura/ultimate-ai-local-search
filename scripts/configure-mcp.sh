#!/usr/bin/env bash
# Safely merge the claude-context MCP server block into an agent config (default ~/.claude.json).
# Only touches the "claude-context" key — your other MCP servers and secrets are preserved.
# Always makes a timestamped backup first.
set -euo pipefail

ENV_FILE=""; CONFIG="${CLAUDE_CONFIG:-$HOME/.claude.json}"
while [ $# -gt 0 ]; do case "$1" in
  --env-file) ENV_FILE="$2"; shift 2;;
  --config)   CONFIG="$2"; shift 2;;
  *) shift;;
esac; done

[ -n "$ENV_FILE" ] && [ -f "$ENV_FILE" ] || { echo "✗ env file not found: $ENV_FILE" >&2; exit 1; }
command -v jq >/dev/null || { echo "✗ jq required" >&2; exit 1; }

# shellcheck disable=SC1090
set -a; . "$ENV_FILE"; set +a

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
cp "$CONFIG" "$backup"

tmp="$(mktemp)"
jq --argjson env "$env_obj" '
  .mcpServers = (.mcpServers // {})
  | .mcpServers."claude-context" = {
      type: "stdio",
      command: "npx",
      args: ["-y", "@zilliz/claude-context-mcp@latest"],
      env: $env
    }
' "$CONFIG" > "$tmp" && mv "$tmp" "$CONFIG"

echo "✓ claude-context MCP written to $CONFIG (backup: $backup)"
echo "  restart your agent to load the new config"
