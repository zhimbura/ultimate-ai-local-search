#!/usr/bin/env bash
# Fails when a file in the git index (what the next commit will contain) holds private data:
#   - an absolute home path such as /Users/<name>/ or /home/<name>/;
#   - a string from the local deny-list.
# Runs as the pre-commit hook (.githooks/pre-commit) and in CI.
#
# Deny-list: one fixed string per line, matched case-insensitively; empty lines and lines that
# start with '#' are skipped. Location: $UALS_PRIVATE_DENYLIST, else ~/.config/uals/private-denylist.
# Never commit the deny-list itself.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
denylist="${UALS_PRIVATE_DENYLIST:-$HOME/.config/uals/private-denylist}"
self=":(exclude)scripts/check-private.sh"
status=0

if git grep --cached -n -I -E '/(Users|home)/[A-Za-z0-9._-]+/' -- . "$self"; then
  echo "✗ absolute home paths found: use \$WORKSPACE, ~ or a relative path" >&2
  status=1
fi

if [[ -f "$denylist" ]]; then
  patterns="$(mktemp)"
  trap 'rm -f "$patterns"' EXIT
  grep -v -e '^[[:space:]]*#' -e '^[[:space:]]*$' "$denylist" > "$patterns" || true
  if [[ -s "$patterns" ]] && git grep --cached -n -I -i -F -f "$patterns" -- . "$self"; then
    echo "✗ strings from the private deny-list found" >&2
    status=1
  fi
else
  echo "! no deny-list at $denylist: checked home paths only" >&2
fi

if [[ $status -eq 0 ]]; then
  echo "✓ no private data in the index"
fi
exit "$status"
