#!/usr/bin/env bash
# Edit the uals entry in a Claude Code config (default ~/.claude.json) with a safe jq merge.
#   configure-mcp.sh --uals-bin <path> [--env-file <file>] [--config <file>] [--remove-claude-context]
#                    [--no-save-previous]
#       Writes .mcpServers.uals. The env of the entry holds only the non-empty provider API keys
#       from the env file. A different uals entry that was there before is saved to
#       removed-mcp/uals.json first (--remove puts it back); --no-save-previous replaces it without
#       saving, for an entry that an earlier install wrote. --remove-claude-context first saves
#       every claude-context entry (.mcpServers and every .projects[*].mcpServers) and then deletes it.
#   configure-mcp.sh --remove [--config <file>]
#       Deletes .mcpServers.uals (entries under .projects[*] are not ours and stay) and puts back
#       the entry saved by the add step. When nothing but an empty mcpServers is left, the config
#       file is deleted (after a backup).
#   configure-mcp.sh --restore-claude-context [--config <file>]
#       Puts the saved claude-context entries back. An entry that exists again is kept as it is.
#       Entries of projects that are not in the config any more stay in the saved file.
# Other keys are never touched. The config is backed up before it is rewritten (the 3 newest
# backups are kept) and replaced atomically; a symlinked config is written through the link.
# Nothing is written when the result equals the current content. Secret values are never printed
# and never passed on a command line.
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/env.sh
. "$REPO_DIR/scripts/lib/env.sh"

UALS_BIN=""; ENV_FILE=""; CONFIG="${CLAUDE_CONFIG:-$HOME/.claude.json}"
MODE=add; REMOVE_CC=0; SAVE_PREVIOUS=1
SAVED_DIR="$HOME/.local/share/uals/removed-mcp"; SAVED="$SAVED_DIR/claude-context.json"
SAVED_UALS="$SAVED_DIR/uals.json"
BACKUPS_KEPT=3
MAX_TRIES=3
KEYS=(OPENAI_API_KEY VOYAGEAI_API_KEY GEMINI_API_KEY OPENROUTER_API_KEY)

die() { echo "✗ $*" >&2; exit 1; }

while [ $# -gt 0 ]; do case "$1" in
  --uals-bin) [ $# -ge 2 ] || die "$1 needs a value"; UALS_BIN="$2"; shift 2;;
  --env-file) [ $# -ge 2 ] || die "$1 needs a value"; ENV_FILE="$2"; shift 2;;
  --config)   [ $# -ge 2 ] || die "$1 needs a value"; CONFIG="$2"; shift 2;;
  --remove-claude-context) REMOVE_CC=1; shift;;
  --no-save-previous) SAVE_PREVIOUS=0; shift;;
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

# Nothing to remove or restore without a config; add creates the file.
if [ ! -f "$CONFIG" ] && [ "$MODE" != add ]; then echo "- no config at $CONFIG: nothing to do"; exit 0; fi
if [ "$MODE" = restore ] && [ ! -f "$SAVED" ]; then echo "- no saved claude-context entries: nothing to restore"; exit 0; fi

work="$(mktemp -d)"; newf=""
trap 'rm -rf "$work"; [ -z "$newf" ] || rm -f "$newf"' EXIT
snap="$work/snap"; out="$work/out"; rest="$work/rest"

# A claude-context entry: the key says so, or the command line names the package.
JQ_DEFS='
  def is_cc($k; $v):
    $k == "claude-context"
    or ($v | type == "object" and ((.args // []) | map(tostring) | any(test("claude-context-mcp"))));
  def cc_only: with_entries(select(is_cc(.key; .value)));
  def cc_drop: with_entries(select(is_cc(.key; .value) | not));
'

# save_claude_context_entries: adds every claude-context entry of $snap to $SAVED (the saved
# entries of an earlier run stay, the new ones win on the same key).
save_claude_context_entries() {
  local saved_new old
  saved_new="$(jq -c "$JQ_DEFS"'
    { mcpServers: ((.mcpServers // {}) | cc_only),
      projects: ((.projects // {}) | with_entries(.value = ((.value.mcpServers // {}) | cc_only))
                 | with_entries(select(.value | length > 0))) }' "$snap")"
  [ "$(jq -r '(.mcpServers | length) + (.projects | length)' <<<"$saved_new")" -gt 0 ] || return 0
  mkdir -p "$SAVED_DIR"; chmod 700 "$SAVED_DIR"
  old='{}'; [ ! -f "$SAVED" ] || old="$(cat "$SAVED")"
  ( umask 077
    jq -n --argjson old "$old" --argjson new "$saved_new" '$old * $new' > "$SAVED.new" )
  mv -f "$SAVED.new" "$SAVED"
  chmod 600 "$SAVED"
  echo "✓ saved $(jq -r '(.mcpServers | length) + ([.projects[] | length] | add // 0)' <<<"$saved_new") claude-context entries to $SAVED"
}

# save_previous_uals_entry: a uals entry that differs from the new one is kept in $SAVED_UALS,
# unless an earlier run already saved one.
save_previous_uals_entry() {
  local prev new
  prev="$(jq -cS '.mcpServers.uals // empty' "$snap")"
  [ -n "$prev" ] || return 0
  new="$(jq -cS '.mcpServers.uals' "$out")"
  [ "$prev" != "$new" ] || return 0
  [ ! -f "$SAVED_UALS" ] || return 0
  mkdir -p "$SAVED_DIR"; chmod 700 "$SAVED_DIR"
  ( umask 077; printf '%s\n' "$prev" > "$SAVED_UALS.new" )
  mv -f "$SAVED_UALS.new" "$SAVED_UALS"
  chmod 600 "$SAVED_UALS"
  echo "✓ the config had a different uals entry: saved to $SAVED_UALS (uninstall.sh puts it back)"
}

# compute: reads $snap, writes the new config to $out (and, for restore, what stays saved to $rest).
compute() {
  case "$MODE" in
  add)
    # The entry env: non-empty provider API keys from the env file only. jq reads them from its
    # environment, so they never appear on a command line.
    local k
    for k in "${KEYS[@]}"; do unset "$k"; done
    [ -z "$ENV_FILE" ] || env_load_file "$ENV_FILE"
    [ "$REMOVE_CC" = 0 ] || save_claude_context_entries
    jq --arg bin "$UALS_BIN" --arg keys "${KEYS[*]}" --argjson drop "$REMOVE_CC" "$JQ_DEFS"'
      def uals_entry: {type: "stdio", command: $bin, args: ["mcp"],
        env: (reduce ($keys | split(" "))[] as $k ({}; if (env[$k] // "") != "" then . + {($k): env[$k]} else . end))};
      (if $drop == 1 then
         (if has("mcpServers") then .mcpServers |= cc_drop else . end)
         | (if has("projects") then .projects |= with_entries(.value |= (if has("mcpServers") then .mcpServers |= cc_drop else . end)) else . end)
       else . end)
      | .mcpServers = (.mcpServers // {})
      | .mcpServers.uals = uals_entry
    ' "$snap" > "$out"
    ;;
  remove)
    local saved_arg=/dev/null
    [ ! -f "$SAVED_UALS" ] || saved_arg="$SAVED_UALS"
    jq --slurpfile s "$saved_arg" '
      (if has("mcpServers") then .mcpServers |= del(.uals) else . end)
      | (if ($s | length) > 0 then .mcpServers = ((.mcpServers // {}) + {uals: $s[0]}) else . end)
    ' "$snap" > "$out"
    ;;
  restore)
    jq --slurpfile s "$SAVED" "$JQ_DEFS"'
      $s[0] as $saved
      | .mcpServers = (($saved.mcpServers // {}) + (.mcpServers // {}))
      | reduce (($saved.projects // {}) | to_entries[]) as $p (.;
          if (.projects // {} | has($p.key)) then
            .projects[$p.key].mcpServers = ($p.value + (.projects[$p.key].mcpServers // {}))
          else . end)
    ' "$snap" > "$out"
    jq --slurpfile c "$snap" '
      {mcpServers: {}, projects: ((.projects // {}) | with_entries(. as $e | select((($c[0].projects // {}) | has($e.key)) | not)))}
    ' "$SAVED" > "$rest"
    ;;
  esac
}

# rotate_backups: keeps the $BACKUPS_KEPT newest backups of the exact form <config>.bak.YYYYmmdd-HHMMSS.
# Anything else (e.g. .bak.20260619-claude-context-fix) is never touched.
rotate_backups() {
  local old_backups=() f
  for f in "${CONFIG}".bak.*; do
    [ -e "$f" ] || continue
    case "${f##*.bak.}" in
      [0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[0-9][0-9][0-9][0-9][0-9][0-9]) old_backups+=("$f");;
    esac
  done
  if [ "${#old_backups[@]}" -gt "$BACKUPS_KEPT" ]; then
    # fixed-width timestamps sort lexically == chronologically
    while IFS= read -r f; do rm -f -- "$f"; done < <(printf '%s\n' "${old_backups[@]}" | sort | head -n "$(( ${#old_backups[@]} - BACKUPS_KEPT ))")
  fi
}

# The edit cycle: copy the config, compute the result from the copy, back up, and write only when
# the config is still what was read. Claude Code rewrites this file too; on a change we start over.
tries=0; backup=""; delete=0; changed=1
while :; do
  tries=$((tries + 1))
  if [ -f "$CONFIG" ]; then
    cp "$CONFIG" "$snap"; had=1
    jq empty "$snap" >/dev/null 2>&1 || die "$CONFIG is not valid JSON: aborting, config left untouched"
  else
    echo '{}' > "$snap"; had=0
  fi
  compute
  [ -s "$out" ] || die "jq produced empty output: $CONFIG left untouched"

  if [ "$had" = 1 ] && jq -en --slurpfile a "$snap" --slurpfile b "$out" '$a[0] == $b[0]' >/dev/null; then
    changed=0; echo "✓ $CONFIG is already up to date"; break
  fi
  if [ "$MODE" = add ] && [ "$SAVE_PREVIOUS" = 1 ]; then save_previous_uals_entry; fi
  delete=0
  if [ "$MODE" = remove ] && jq -e '(del(.mcpServers) == {}) and ((.mcpServers // {}) == {})' "$out" >/dev/null 2>&1; then delete=1; fi

  backup=""
  if [ "$had" = 1 ]; then
    stamp="$(date +%Y%m%d-%H%M%S)"
    while [ -e "${CONFIG}.bak.$stamp" ]; do sleep 1; stamp="$(date +%Y%m%d-%H%M%S)"; done
    backup="${CONFIG}.bak.$stamp"
    cp -p "$CONFIG" "$backup"
  fi

  if { [ "$had" = 1 ] && cmp -s "$snap" "$CONFIG"; } || { [ "$had" = 0 ] && [ ! -e "$CONFIG" ]; }; then
    if [ "$delete" = 1 ]; then
      rm -f -- "$CONFIG"
    else
      mkdir -p "$(dirname "$CONFIG")"
      newf="$(mktemp "$(dirname "$CONFIG")/.$(basename "$CONFIG").XXXXXX")"   # mode 600
      [ "$had" = 0 ] || cp -p "$CONFIG" "$newf"                                # the old file's mode
      cat "$out" > "$newf"
      mv -f "$newf" "$CONFIG"; newf=""
    fi
    break
  fi
  [ -z "$backup" ] || rm -f "$backup"
  [ "$tries" -lt "$MAX_TRIES" ] || die "$CONFIG keeps changing (is Claude Code running?): close the agent and run again"
  echo "! $CONFIG changed while it was being edited: starting over" >&2
  sleep 1
done

if [ "$changed" = 1 ]; then
  [ -z "$backup" ] || rotate_backups
  case "$MODE" in
    add)     echo "✓ uals MCP entry written to $CONFIG${backup:+ (backup: $backup)}"
             echo "  restart your agent to load the new config";;
    remove)  if [ "$delete" = 1 ]; then echo "✓ uals MCP entry removed: $CONFIG held nothing else, so the file is deleted (backup: $backup)"
             else echo "✓ uals MCP entry removed from $CONFIG (backup: $backup)"; fi;;
    restore) echo "✓ claude-context entries restored in $CONFIG (backup: $backup)";;
  esac
fi

case "$MODE" in
remove)
  if [ -f "$SAVED_UALS" ]; then
    rm -f -- "$SAVED_UALS"; rmdir "$SAVED_DIR" 2>/dev/null || true
    echo "✓ put back the uals entry that was in the config before the install"
  fi
  ;;
restore)
  left="$(jq -r '[.projects[] | length] | add // 0' "$rest")"
  if [ "$left" = 0 ]; then
    rm -f -- "$SAVED"; rmdir "$SAVED_DIR" 2>/dev/null || true
  else
    ( umask 077; cat "$rest" > "$SAVED.new" )
    mv -f "$SAVED.new" "$SAVED"; chmod 600 "$SAVED"
    echo "! $left saved claude-context entries of $(jq -r '.projects | length' "$rest") projects that are not in $CONFIG any more were not restored: they stay in $SAVED" >&2
  fi
  ;;
esac
