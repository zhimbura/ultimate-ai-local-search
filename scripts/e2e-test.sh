#!/usr/bin/env bash
# End-to-end acceptance test: create a tiny project → index it via the real
# claude-context MCP → semantic search → assert → clean up (index + files).
# Needs Node.js 18+ (the MCP server runs via npx). Run AFTER ./install.sh.
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$REPO_DIR/.env"

[ -f "$ENV_FILE" ] || { echo "✗ no .env — run ./install.sh first" >&2; exit 1; }
command -v node >/dev/null || { echo "✗ Node.js 18+ required (claude-context runs via npx). Install node and retry." >&2; exit 1; }

# export config so the spawned claude-context MCP picks it up
# shellcheck disable=SC1090
set -a; . "$ENV_FILE"; set +a

TMP="$(mktemp -d)"
FIX="$TMP/sample-project"
mkdir -p "$FIX"
cat > "$FIX/retry.js" <<'EOF'
// Network helper with exponential backoff retry.
async function fetchWithRetry(url, maxAttempts = 5) {
  let delay = 200;
  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try { return await fetch(url); }
    catch (err) {
      if (attempt === maxAttempts) throw err;
      await new Promise((r) => setTimeout(r, delay));
      delay *= 2; // exponential backoff
    }
  }
}
module.exports = { fetchWithRetry };
EOF
cat > "$FIX/auth.js" <<'EOF'
// Authenticate a user and issue a session token.
function login(username, password) { /* verify credentials, return JWT */ }
module.exports = { login };
EOF
cat > "$FIX/math.py" <<'EOF'
def factorial(n):
    return 1 if n <= 1 else n * factorial(n - 1)
EOF

echo "▸ e2e fixture project: $FIX (retry.js / auth.js / math.py)"
# Snapshot pre-existing MCP processes so we only clean up the one THIS test spawns
# (never touch a real claude-context MCP your agent may be running).
before="$(pgrep -f 'claude-context-mcp' 2>/dev/null | sort -u || true)"
set +e
node "$REPO_DIR/test/e2e.mjs" "$FIX" "retry a network request with exponential backoff" "retry.js"
rc=$?
set -e
after="$(pgrep -f 'claude-context-mcp' 2>/dev/null | sort -u || true)"
newpids="$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") 2>/dev/null | tr '\n' ' ')"
[ -n "${newpids// /}" ] && kill -9 $newpids 2>/dev/null || true
rm -rf "$TMP"
exit "$rc"
