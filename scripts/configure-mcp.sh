#!/usr/bin/env bash
# Edit the uals entry in a Claude Code config (default ~/.claude.json) with a safe jq merge.
#   configure-mcp.sh --uals-bin <path> [--env-file <file>] [--config <file>] [--remove-claude-context]
#       Writes .mcpServers.uals. The env of the entry holds only the non-empty provider API keys
#       from the env file. --remove-claude-context first saves every claude-context entry
#       (.mcpServers and every .projects[*].mcpServers) and then deletes it.
#   configure-mcp.sh --remove [--config <file>]
#       Deletes the uals entries (.mcpServers.uals and every .projects[*].mcpServers.uals).
#   configure-mcp.sh --restore-claude-context [--config <file>]
#       Puts the saved claude-context entries back. An entry that exists again is kept as it is.
# Other keys are never touched. The config is backed up before it is rewritten (the 3 newest
# backups are kept). A symlinked config is written through the link. Nothing is written when the
# result equals the current content. Secret values are never printed.
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/env.sh
. "$REPO_DIR/scripts/lib/env.sh"

UALS_BIN=""; ENV_FILE=""; CONFIG="${CLAUDE_CONFIG:-$HOME/.claude.json}"
MODE=add; REMOVE_CC=0
SAVED_DIR="$HOME/.local/share/uals/removed-mcp"; SAVED="$SAVED_DIR/claude-context.json"

die() { echo "✗ $*" >&2; exit 1; }

while [ $# -gt 0 ]; do case "$1" in
  --uals-bin) [ $# -ge 2 ] || die "$1 needs a value"; UALS_BIN="$2"; shift 2;;
  --env-file) [ $# -ge 2 ] || die "$1 needs a value"; ENV_FILE="$2"; shift 2;;
  --config)   [ $# -ge 2 ] || die "$1 needs a value"; CONFIG="$2"; shift 2;;
  --remove-claude-context) REMOVE_CC=1; shift;;
  --remove) MODE=remove; shift;;
  --restore-claude-context) MODE=restore; shift;;
  *) die "unknown flag: $1";;
esac; done

command -v jq >/dev/null || die "jq required"
if [ "$MODE" = add ]; then
  case "$UALS_BIN" in /*) ;; *) die "--uals-bin must be an absolute path";; esac
  [ -z "$ENV_FILE" ] || [ -f "$ENV_FILE" ] || die "env file not found: $ENV_FILE"
fi

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

if [ ! -f "$CONFIG" ]; then
  # Nothing to remove or restore; add creates the file.
  [ "$MODE" = add ] || { echo "- no config at $CONFIG: nothing to do"; exit 0; }
fi
if [ -f "$CONFIG" ]; then
  jq empty "$CONFIG" >/dev/null 2>&1 || die "$CONFIG is not valid JSON: aborting, config left untouched"
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# A claude-context entry: the key says so, or the command line names the package.
JQ_DEFS='
  def is_cc($k; $v):
    $k == "claude-context"
    or ($v | type == "object" and ((.args // []) | map(tostring) | any(test("claude-context-mcp"))));
  def cc_only: with_entries(select(is_cc(.key; .value)));
  def cc_drop: with_entries(select(is_cc(.key; .value) | not));
  def is_uals($k; $v): $k == "uals";
'

current="$CONFIG"
if [ ! -f "$current" ]; then current="$(mktemp)"; echo '{}' > "$current"; created_empty=1; else created_empty=0; fi

case "$MODE" in
add)
  # The entry env: non-empty provider API keys from the env file only.
  keys=(OPENAI_API_KEY VOYAGEAI_API_KEY GEMINI_API_KEY OPENROUTER_API_KEY)
  for k in "${keys[@]}"; do unset "$k"; done
  [ -z "$ENV_FILE" ] || env_load_file "$ENV_FILE"
  env_obj='{}'
  for k in "${keys[@]}"; do
    if [ -n "${!k:-}" ]; then env_obj="$(jq -cn --argjson o "$env_obj" --arg k "$k" --arg v "${!k}" '$o + {($k): $v}')"; fi
  done

  if [ "$REMOVE_CC" = 1 ]; then
    saved_new="$(jq -c "$JQ_DEFS"'
      { mcpServers: ((.mcpServers // {}) | cc_only),
        projects: ((.projects // {}) | with_entries(.value = ((.value.mcpServers // {}) | cc_only))
                   | with_entries(select(.value | length > 0))) }' "$current")"
    if [ "$(jq -r '(.mcpServers | length) + (.projects | length)' <<<"$saved_new")" -gt 0 ]; then
      mkdir -p "$SAVED_DIR"; chmod 700 "$SAVED_DIR"
      old='{}'; [ ! -f "$SAVED" ] || old="$(cat "$SAVED")"
      umask 077
      jq -n --argjson old "$old" --argjson new "$saved_new" '$old * $new' > "$SAVED.new"
      mv -f "$SAVED.new" "$SAVED"
      chmod 600 "$SAVED"
      echo "✓ saved $(jq -r '(.mcpServers | length) + ([.projects[] | length] | add // 0)' <<<"$saved_new") claude-context entries to $SAVED"
    fi
  fi

  jq --arg bin "$UALS_BIN" --argjson env "$env_obj" --argjson drop "$REMOVE_CC" "$JQ_DEFS"'
    (if $drop == 1 then
       (if has("mcpServers") then .mcpServers |= cc_drop else . end)
       | (if has("projects") then .projects |= with_entries(.value |= (if has("mcpServers") then .mcpServers |= cc_drop else . end)) else . end)
     else . end)
    | .mcpServers = (.mcpServers // {})
    | .mcpServers.uals = {type: "stdio", command: $bin, args: ["mcp"], env: $env}
  ' "$current" > "$tmp"
  ;;
remove)
  jq "$JQ_DEFS"'
    (if has("mcpServers") then .mcpServers |= with_entries(select(is_uals(.key; .value) | not)) else . end)
    | (if has("projects") then .projects |= with_entries(.value |= (if has("mcpServers") then .mcpServers |= with_entries(select(is_uals(.key; .value) | not)) else . end)) else . end)
  ' "$current" > "$tmp"
  ;;
restore)
  [ -f "$SAVED" ] || { echo "- no saved claude-context entries: nothing to restore"; exit 0; }
  jq --slurpfile s "$SAVED" "$JQ_DEFS"'
    $s[0] as $saved
    | .mcpServers = (($saved.mcpServers // {}) + (.mcpServers // {}))
    | reduce (($saved.projects // {}) | to_entries[]) as $p (.;
        if (.projects // {} | has($p.key)) then
          .projects[$p.key].mcpServers = ($p.value + (.projects[$p.key].mcpServers // {}))
        else . end)
  ' "$current" > "$tmp"
  ;;
esac
[ "$created_empty" = 0 ] || rm -f "$current"
[ -s "$tmp" ] || die "jq produced empty output: $CONFIG left untouched"

if [ -f "$CONFIG" ] && jq -en --slurpfile a "$CONFIG" --slurpfile b "$tmp" '$a[0] == $b[0]' >/dev/null; then
  echo "✓ $CONFIG is already up to date"
else
  backup=""
  if [ -f "$CONFIG" ]; then
    stamp="$(date +%Y%m%d-%H%M%S)"
    while [ -e "${CONFIG}.bak.$stamp" ]; do sleep 1; stamp="$(date +%Y%m%d-%H%M%S)"; done
    backup="${CONFIG}.bak.$stamp"
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
  else
    mkdir -p "$(dirname "$CONFIG")"
  fi
  cat "$tmp" > "$CONFIG"   # write in place: keeps symlink, inode and permissions
  case "$MODE" in
    add)     echo "✓ uals MCP entry written to $CONFIG${backup:+ (backup: $backup)}";;
    remove)  echo "✓ uals MCP entry removed from $CONFIG (backup: $backup)";;
    restore) echo "✓ claude-context entries restored in $CONFIG (backup: $backup)";;
  esac
  [ "$MODE" != add ] || echo "  restart your agent to load the new config"
fi

if [ "$MODE" = restore ]; then
  rm -f -- "$SAVED"; rmdir "$SAVED_DIR" 2>/dev/null || true
fi
