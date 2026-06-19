#!/usr/bin/env bash
# Tier 1 acceptance test: ast-index builds an index and finds known symbols.
# Creates a sample project → ast-index rebuild → symbol search → assert → clean up.
set -euo pipefail

if ! command -v ast-index >/dev/null; then
  echo "✗ ast-index not installed — Tier 1 unavailable." >&2
  echo "  install.sh installs it (macOS: brew · Linux: prebuilt from github.com/defendend/Claude-ast-index-search/releases)." >&2
  exit 1
fi
echo "── ast-index (Tier 1) test ────────────────────────────────"
echo "▸ $(ast-index version 2>/dev/null || ast-index --version 2>/dev/null || echo ast-index)"

TMP="$(mktemp -d)"; FIX="$TMP/sample-project"; mkdir -p "$FIX"
cat > "$FIX/retry.js" <<'EOF'
async function fetchWithRetry(url, maxAttempts = 5) {
  let delay = 200;
  for (let i = 1; i <= maxAttempts; i++) {
    try { return await fetch(url); } catch (e) { if (i === maxAttempts) throw e; delay *= 2; }
  }
}
module.exports = { fetchWithRetry };
EOF
cat > "$FIX/math.py" <<'EOF'
def factorial(n):
    return 1 if n <= 1 else n * factorial(n - 1)
EOF

cd "$FIX"
fail=0
echo "▸ ast-index rebuild"
ast-index rebuild >/dev/null 2>&1 || { echo "✗ rebuild failed" >&2; fail=1; }

assert_finds() { # <query> <expected-regex>
  local q="$1" pat="$2" out
  out="$(ast-index search "$q" 2>/dev/null || true)"
  if printf '%s' "$out" | grep -qiE "$pat"; then echo "✓ found '$q'"; else echo "✗ '$q' not found" >&2; printf '%s\n' "$out" | head -3 >&2; fail=1; fi
}
assert_finds "fetchWithRetry" "fetchWithRetry|retry\.js"
assert_finds "factorial" "factorial|math\.py"

ast-index clear >/dev/null 2>&1 || true   # drop the index for this fixture
cd /; rm -rf "$TMP"

echo "───────────────────────────────────────────────────────────"
if [ "$fail" = 0 ]; then echo "✓ ast-index test passed"; else echo "✗ ast-index test failed" >&2; fi
exit "$fail"
