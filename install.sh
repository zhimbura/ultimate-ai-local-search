#!/usr/bin/env bash
# ultimate-ai-local-search — one-command local code search for AI agents.
# Tiers: ast-index (symbols) + claude-context/Milvus (semantic) + rg/grep (text).
# macOS + Linux. Idempotent: safe to re-run.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_DIR"

# ─── pretty output ───────────────────────────────────────────────────────────
if [ -t 1 ]; then B=$'\033[1m'; G=$'\033[32m'; Y=$'\033[33m'; R=$'\033[31m'; C=$'\033[36m'; N=$'\033[0m'; else B=; G=; Y=; R=; C=; N=; fi
say()  { printf '%s\n' "${C}▸${N} $*"; }
ok()   { printf '%s\n' "${G}✓${N} $*"; }
warn() { printf '%s\n' "${Y}!${N} $*" >&2; }
die()  { printf '%s\n' "${R}✗ $*${N}" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# ─── flags / non-interactive ─────────────────────────────────────────────────
PROVIDER="${PROVIDER:-}"; MODEL="${MODEL:-}"; DIMENSION="${DIMENSION:-}"
ASSUME_YES="${ASSUME_YES:-0}"; DO_SMOKE=1; DO_AST=1
while [ $# -gt 0 ]; do case "$1" in
  --provider) PROVIDER="$2"; shift 2;;
  --model) MODEL="$2"; shift 2;;
  --dimension) DIMENSION="$2"; shift 2;;
  --yes|-y) ASSUME_YES=1; shift;;
  --no-smoke) DO_SMOKE=0; shift;;
  --skip-ast-index) DO_AST=0; shift;;
  -h|--help) grep -E '^# ' "$0" | sed 's/^# //'; exit 0;;
  *) die "unknown flag: $1";;
esac; done

OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS" in Darwin) PLATFORM=macos;; Linux) PLATFORM=linux;; *) die "unsupported OS: $OS (use install.ps1 on Windows)";; esac
say "${B}ultimate-ai-local-search${N} — platform: $PLATFORM/$ARCH"

# ─── prerequisites ───────────────────────────────────────────────────────────
ensure_pkg() { # ensure_pkg <cmd> <brew-formula> [linux-apt-pkg]
  local cmd="$1" brew_f="$2" apt_p="${3:-$1}"
  have "$cmd" && return 0
  say "installing $cmd…"
  if have brew; then brew install "$brew_f"
  elif [ "$PLATFORM" = linux ] && have apt-get; then sudo apt-get update -qq && sudo apt-get install -y "$apt_p"
  else die "can't install '$cmd' automatically — install it manually and re-run"; fi
}
ensure_pkg jq jq
ensure_pkg curl curl
have docker || die "Docker not found. Install Docker Desktop / OrbStack / Docker Engine, then re-run."
docker info >/dev/null 2>&1 || die "Docker is installed but not running. Start it and re-run."
docker compose version >/dev/null 2>&1 || die "Docker Compose v2 required (docker compose ...)."
ok "docker + jq + curl ready"

# Node is NOT used by this installer, but the claude-context MCP server is launched
# by your agent via `npx` — warn (don't fail) if it's missing so semantic search works later.
if have node && have npx; then
  ok "node present ($(node --version 2>/dev/null))"
else
  warn "Node.js/npx not found — not needed for THIS install, but REQUIRED afterward:"
  warn "  your AI agent runs the claude-context MCP via 'npx @zilliz/claude-context-mcp'."
  warn "  Install Node.js 18+ (brew install node · https://nodejs.org) before using semantic search,"
  warn "  otherwise the Milvus stack will be up but the MCP server won't start."
fi

# ─── Tier 1: ast-index (structural) ──────────────────────────────────────────
if [ "$DO_AST" = 1 ]; then
  if have ast-index; then ok "ast-index present ($(ast-index --version 2>/dev/null || echo ok))"
  elif have brew; then
    say "installing ast-index (Tier 1: structural symbol search)…"
    brew tap defendend/ast-index >/dev/null 2>&1 || true
    brew install ast-index && brew trust defendend/ast-index/ast-index 2>/dev/null || warn "ast-index install failed — Tier 1 optional, continuing"
  else
    warn "Homebrew not found — skipping ast-index (Tier 1). Install brew or grab ast-index manually later."
  fi
fi

# ─── provider selection ──────────────────────────────────────────────────────
if [ -z "$PROVIDER" ]; then
  cat <<EOF

Where should embeddings be computed?
  ${B}Local${N} (private, free, offline):
    1) Ollama       — headless, simplest
    2) LM Studio    — via lms CLI + REST :1234
  ${B}Cloud${N} (needs an API key):
    3) OpenAI       — text-embedding-3-small/large
    4) VoyageAI     — voyage-code-3 (best for code)
    5) Gemini       — text-embedding-004
    6) OpenRouter   — proxy to many providers
EOF
  read -rp "Choose [1-6] (default 1): " choice
  case "${choice:-1}" in
    1) PROVIDER=ollama;; 2) PROVIDER=lmstudio;; 3) PROVIDER=openai;;
    4) PROVIDER=voyageai;; 5) PROVIDER=gemini;; 6) PROVIDER=openrouter;;
    *) die "invalid choice";;
  esac
fi

# defaults per provider: MODEL + DIMENSION + claude-context EMBEDDING_PROVIDER
EMB_PROVIDER=""; OPENAI_BASE_URL=""; OLLAMA_HOST="${OLLAMA_HOST:-http://127.0.0.1:11434}"
case "$PROVIDER" in
  ollama)     EMB_PROVIDER=Ollama;  MODEL="${MODEL:-nomic-embed-text}";               DIMENSION="${DIMENSION:-768}";;
  lmstudio)   EMB_PROVIDER=OpenAI;  MODEL="${MODEL:-text-embedding-nomic-embed-text-v1.5}"; DIMENSION="${DIMENSION:-768}"; OPENAI_BASE_URL="http://127.0.0.1:1234/v1";;
  openai)     EMB_PROVIDER=OpenAI;  MODEL="${MODEL:-text-embedding-3-small}";          DIMENSION="${DIMENSION:-1536}";;
  voyageai)   EMB_PROVIDER=VoyageAI;MODEL="${MODEL:-voyage-code-3}";                   DIMENSION="${DIMENSION:-1024}";;
  gemini)     EMB_PROVIDER=Gemini;  MODEL="${MODEL:-text-embedding-004}";              DIMENSION="${DIMENSION:-768}";;
  openrouter) EMB_PROVIDER=OpenRouter; MODEL="${MODEL:-openai/text-embedding-3-small}";DIMENSION="${DIMENSION:-1536}";;
  *) die "unknown provider: $PROVIDER";;
esac
say "provider=${B}$PROVIDER${N} model=${B}$MODEL${N} dim=${B}$DIMENSION${N}"

# ─── provider setup (local install / cloud key prompt) ───────────────────────
API_KEY_VAR=""; API_KEY_VAL=""
case "$PROVIDER" in
  ollama)
    if ! have ollama; then
      say "installing Ollama…"
      if [ "$PLATFORM" = macos ] && have brew; then brew install ollama; ( ollama serve >/dev/null 2>&1 & )
      else curl -fsSL https://ollama.com/install.sh | sh; fi
    fi
    have ollama && ! curl -fsS --noproxy '*' "$OLLAMA_HOST/api/tags" >/dev/null 2>&1 && ( ollama serve >/dev/null 2>&1 & ) && sleep 2 || true
    say "pulling model $MODEL…"; ollama pull "$MODEL"
    ;;
  lmstudio)
    LMS="$HOME/.lmstudio/bin/lms"
    have lms && LMS=lms
    if [ ! -x "$LMS" ] && ! have lms; then
      if [ "$PLATFORM" = macos ] && have brew; then brew install --cask lm-studio || die "install LM Studio manually: https://lmstudio.ai"
      else die "LM Studio: install from https://lmstudio.ai then re-run (need 'lms' CLI)"; fi
      LMS="$HOME/.lmstudio/bin/lms"
    fi
    say "downloading model + starting LM Studio server…"
    "$LMS" get "$MODEL" 2>/dev/null || warn "could not auto-download '$MODEL' — load it in LM Studio manually"
    "$LMS" server start 2>/dev/null || warn "could not start LM Studio server — start it manually (port 1234)"
    ;;
  openai)     API_KEY_VAR=OPENAI_API_KEY;     API_KEY_VAL="${OPENAI_API_KEY:-}";;
  voyageai)   API_KEY_VAR=VOYAGEAI_API_KEY;   API_KEY_VAL="${VOYAGEAI_API_KEY:-}";;
  gemini)     API_KEY_VAR=GEMINI_API_KEY;     API_KEY_VAL="${GEMINI_API_KEY:-}";;
  openrouter) API_KEY_VAR=OPENROUTER_API_KEY; API_KEY_VAL="${OPENROUTER_API_KEY:-}";;
esac
if [ -n "$API_KEY_VAR" ] && [ -z "$API_KEY_VAL" ]; then
  read -rsp "Enter $API_KEY_VAR: " API_KEY_VAL; echo
  [ -n "$API_KEY_VAL" ] || die "$API_KEY_VAR is required for $PROVIDER"
fi

# ─── write .env ──────────────────────────────────────────────────────────────
say "writing .env"
{
  echo "MILVUS_PORT=${MILVUS_PORT:-19530}"
  echo "MILVUS_HEALTH_PORT=${MILVUS_HEALTH_PORT:-9091}"
  echo "MILVUS_VERSION=${MILVUS_VERSION:-v2.5.6}"
  echo "ETCD_VERSION=${ETCD_VERSION:-v3.5.18}"
  echo "MINIO_VERSION=${MINIO_VERSION:-RELEASE.2023-03-20T20-16-18Z}"
  echo "MILVUS_ADDRESS=127.0.0.1:${MILVUS_PORT:-19530}"
  echo "EMBEDDING_PROVIDER=$EMB_PROVIDER"
  echo "EMBEDDING_MODEL=$MODEL"
  echo "EMBEDDING_DIMENSION=$DIMENSION"
  [ -n "$OPENAI_BASE_URL" ] && echo "OPENAI_BASE_URL=$OPENAI_BASE_URL"
  [ "$PROVIDER" = lmstudio ] && echo "OPENAI_API_KEY=lm-studio"
  [ "$PROVIDER" = ollama ] && echo "OLLAMA_HOST=$OLLAMA_HOST"
  [ -n "$API_KEY_VAR" ] && echo "$API_KEY_VAR=$API_KEY_VAL"
} > "$REPO_DIR/.env"
chmod 600 "$REPO_DIR/.env"
ok ".env written (chmod 600, gitignored)"

# ─── start Milvus ────────────────────────────────────────────────────────────
say "starting Milvus stack (etcd + minio + milvus)…"
docker compose --env-file "$REPO_DIR/.env" up -d
say "waiting for Milvus to become healthy (first run can take ~1–2 min)…"
MILVUS_HEALTH_PORT="${MILVUS_HEALTH_PORT:-9091}"
for i in $(seq 1 60); do
  if curl -fsS --noproxy '*' "http://127.0.0.1:${MILVUS_HEALTH_PORT}/healthz" >/dev/null 2>&1; then ok "Milvus healthy"; break; fi
  [ "$i" = 60 ] && die "Milvus did not become healthy. Check: docker compose logs milvus"
  sleep 3
done

# ─── configure claude-context MCP (safe jq merge into ~/.claude.json) ────────
bash "$REPO_DIR/scripts/configure-mcp.sh" --env-file "$REPO_DIR/.env"

# ─── smoke test (catches the dim-mismatch class of bugs at install time) ─────
if [ "$DO_SMOKE" = 1 ]; then
  bash "$REPO_DIR/scripts/smoke-test.sh" --env-file "$REPO_DIR/.env" || warn "smoke test reported issues (see above)"
fi

cat <<EOF

${G}${B}Done.${N}
Next steps:
  1) ${B}Restart your AI agent${N} (Claude Code / etc.) so it picks up the new MCP env.
  2) Index a project — in the agent, call claude-context ${B}index_codebase${N} on its path,
     or check status with ${B}get_indexing_status${N}.
  3) Search semantically via ${B}search_code${N}; use ast-index for symbols, rg/grep for text.

Verify end to end (needs Node.js): ${B}./scripts/e2e-test.sh${N} — indexes a sample project via claude-context, searches, cleans up.
Rules for agents: see ${B}rules/${N} (3-tier search policy) — copy into your project's AGENTS.md / CLAUDE.md.
Manage the stack: ${B}docker compose --env-file .env {ps,logs,down}${N} · uninstall: ${B}./uninstall.sh${N}
EOF
