#!/usr/bin/env bash
# Tear down the stack. By default keeps your indexed data (Docker volumes).
#   ./uninstall.sh            stop containers, remove claude-context MCP entry
#   ./uninstall.sh --purge    also delete Milvus volumes (indexed vectors) — irreversible
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; cd "$REPO_DIR"
PURGE=0; [ "${1:-}" = "--purge" ] && PURGE=1
ENV_ARGS=(); [ -f .env ] && ENV_ARGS=(--env-file .env)

if [ "$PURGE" = 1 ]; then
  echo "Stopping stack and DELETING volumes (indexed data)…"
  docker compose "${ENV_ARGS[@]}" down --volumes || true
else
  echo "Stopping stack (volumes kept)…"
  docker compose "${ENV_ARGS[@]}" down || true
fi

CONFIG="${CLAUDE_CONFIG:-$HOME/.claude.json}"
if [ -f "$CONFIG" ] && command -v jq >/dev/null && jq -e '.mcpServers."claude-context"' "$CONFIG" >/dev/null 2>&1; then
  cp "$CONFIG" "${CONFIG}.bak.$(date +%Y%m%d-%H%M%S)"
  tmp="$(mktemp)"; jq 'del(.mcpServers."claude-context")' "$CONFIG" > "$tmp" && mv "$tmp" "$CONFIG"
  echo "✓ removed claude-context MCP entry from $CONFIG (backup made)"
fi
echo "Done. (Ollama / LM Studio / ast-index left installed — remove them yourself if you want.)"
