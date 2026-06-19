#!/usr/bin/env bash
# Smoke test: catch the #1 silent failure — model dimension != configured dimension —
# AND verify Milvus actually accepts inserts/search at that dimension.
# A wrong dim makes every insert fail silently; this surfaces it in seconds.
set -euo pipefail

ENV_FILE=""
while [ $# -gt 0 ]; do case "$1" in --env-file) ENV_FILE="$2"; shift 2;; *) shift;; esac; done
[ -n "$ENV_FILE" ] && [ -f "$ENV_FILE" ] || { echo "✗ env file not found: $ENV_FILE" >&2; exit 1; }
# shellcheck disable=SC1090
set -a; . "$ENV_FILE"; set +a

# proxy-safe curl: '*' is QUOTED inside the function so the shell never globs it
# against files in the cwd (that bug skipped real requests in v1 of this script).
_curl() { curl -fsS --noproxy '*' -m 60 "$@"; }

MA="${MILVUS_ADDRESS:-127.0.0.1:19530}"
DIM="${EMBEDDING_DIMENSION:-}"
fail=0
echo "── smoke test ─────────────────────────────────────────────"

# ── Check A: real embedding dimension vs configured EMBEDDING_DIMENSION ──────
actual_dim=""
case "${EMBEDDING_PROVIDER:-}" in
  Ollama)
    actual_dim=$(_curl "${OLLAMA_HOST:-http://127.0.0.1:11434}/api/embeddings" \
      -H 'Content-Type: application/json' \
      -d "{\"model\":\"$EMBEDDING_MODEL\",\"prompt\":\"test\"}" 2>/dev/null | jq -r '.embedding|length' 2>/dev/null || true);;
  OpenAI|OpenRouter)
    base="${OPENAI_BASE_URL:-https://api.openai.com/v1}"
    [ "${EMBEDDING_PROVIDER}" = OpenRouter ] && base="${OPENAI_BASE_URL:-https://openrouter.ai/api/v1}"
    actual_dim=$(_curl "$base/embeddings" \
      -H 'Content-Type: application/json' -H "Authorization: Bearer ${OPENAI_API_KEY:-x}" \
      -d "{\"model\":\"$EMBEDDING_MODEL\",\"input\":\"test\"}" 2>/dev/null | jq -r '.data[0].embedding|length' 2>/dev/null || true);;
  VoyageAI)
    actual_dim=$(_curl "https://api.voyageai.com/v1/embeddings" \
      -H 'Content-Type: application/json' -H "Authorization: Bearer ${VOYAGEAI_API_KEY:-x}" \
      -d "{\"model\":\"$EMBEDDING_MODEL\",\"input\":[\"test\"]}" 2>/dev/null | jq -r '.data[0].embedding|length' 2>/dev/null || true);;
  *) echo "• dim auto-check skipped for provider '${EMBEDDING_PROVIDER:-?}' (verify manually)";;
esac

if [ -n "$actual_dim" ] && [ "$actual_dim" != "null" ]; then
  if [ "$actual_dim" = "$DIM" ]; then
    echo "✓ embedding dim matches: model=$EMBEDDING_MODEL → $actual_dim == EMBEDDING_DIMENSION"
  else
    echo "✗ DIMENSION MISMATCH: model '$EMBEDDING_MODEL' returns $actual_dim but EMBEDDING_DIMENSION=$DIM" >&2
    echo "  → fix EMBEDDING_DIMENSION=$actual_dim in .env and re-run, or inserts will silently fail." >&2
    DIM="$actual_dim"; fail=1
  fi
elif [ -n "${EMBEDDING_PROVIDER:-}" ]; then
  echo "! could not fetch a test embedding (provider/model/key reachable?) — skipping dim compare" >&2
fi

# ── Check B: Milvus accepts a collection at DIM, insert + search work ────────
COL="uals_smoke_$$"
if [ -n "$DIM" ] && [ "$DIM" != "null" ]; then
  vec=$(jq -nc --argjson d "$DIM" '[range(0;$d)|0.1]')
  _curl "http://$MA/v2/vectordb/collections/create" -H 'Content-Type: application/json' \
    -d "{\"collectionName\":\"$COL\",\"dimension\":$DIM,\"metricType\":\"COSINE\",\"autoID\":true}" >/dev/null 2>&1 \
    && echo "✓ Milvus created test collection (dim=$DIM)" || { echo "✗ Milvus create failed" >&2; fail=1; }

  _curl "http://$MA/v2/vectordb/entities/insert" -H 'Content-Type: application/json' \
    -d "{\"collectionName\":\"$COL\",\"data\":[{\"vector\":$vec},{\"vector\":$vec},{\"vector\":$vec}]}" >/dev/null 2>&1 \
    && echo "✓ Milvus accepted inserts at dim=$DIM" || { echo "✗ Milvus insert REJECTED at dim=$DIM (this is the dim-mismatch bug)" >&2; fail=1; }

  # Strong consistency so freshly-inserted rows are visible; retry a few times.
  hits=0
  for _ in 1 2 3 4 5; do
    hits=$(_curl "http://$MA/v2/vectordb/entities/search" -H 'Content-Type: application/json' \
      -d "{\"collectionName\":\"$COL\",\"data\":[$vec],\"annsField\":\"vector\",\"limit\":3,\"consistencyLevel\":\"Strong\"}" 2>/dev/null \
      | jq -r '.data|length' 2>/dev/null || echo 0)
    case "$hits" in ''|*[!0-9]*) hits=0;; esac
    [ "$hits" -gt 0 ] && break
    sleep 2
  done
  if [ "$hits" -gt 0 ]; then echo "✓ Milvus search returned $hits hits — pipeline works end to end"; else echo "✗ Milvus search returned 0" >&2; fail=1; fi

  _curl "http://$MA/v2/vectordb/collections/drop" -H 'Content-Type: application/json' \
    -d "{\"collectionName\":\"$COL\"}" >/dev/null 2>&1 || true
fi

echo "───────────────────────────────────────────────────────────"
if [ "$fail" = 0 ]; then echo "✓ smoke test passed"; else echo "✗ smoke test found problems (see above)" >&2; fi
exit "$fail"
