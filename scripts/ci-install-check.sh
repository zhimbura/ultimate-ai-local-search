#!/usr/bin/env bash
# CI check of the install contract, run from a checkout against a packed release:
# install twice (the second run changes nothing), doctor passes, uninstall restores the state.
#   ci-install-check.sh <download-base> <tag> [work-dir]
# <download-base>/<tag>/ holds the archive from package-release.sh. Changes the real HOME:
# run it on a CI runner only.
set -euo pipefail

[ $# -ge 2 ] || { echo "usage: $0 <download-base> <tag> [work-dir]" >&2; exit 2; }
base="$1"; tag="$2"
work="${3:-$(mktemp -d)}"
mkdir -p "$work"
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$HOME/.local/share/uals/install-manifest"

step() { echo "::group::$*"; }
end_step() { echo "::endgroup::"; }
fail() { echo "✗ $*" >&2; exit 1; }

# Files an install may create or change, and the Claude Code config with its backups.
snapshot_files()  { find "$HOME/.local" "$HOME/.config" -type f 2>/dev/null | sort || true; }
snapshot_claude() { find "$HOME" -maxdepth 1 -name '.claude.json*' 2>/dev/null | sort || true; }

install_uals() {
  UALS_DOWNLOAD_BASE="$base" "$repo_dir/install.sh" --yes --version "$tag" --semantic semble
}

files_before="$work/files.before";   snapshot_files  > "$files_before"
claude_before="$work/claude.before"; snapshot_claude > "$claude_before"

step "Install"
install_uals
cp "$manifest" "$work/manifest.first"
snapshot_claude > "$work/claude.first"
end_step

step "Install again: the manifest and the backups stay as they are"
install_uals
cmp "$manifest" "$work/manifest.first" || fail "the second install changed the manifest"
snapshot_claude > "$work/claude.second"
cmp "$work/claude.first" "$work/claude.second" || fail "the second install left new ~/.claude.json backups"
end_step

step "Doctor"
"$HOME/.local/bin/uals" doctor
end_step

step "Uninstall: only new ~/.claude.json backups may remain"
"$repo_dir/uninstall.sh" --yes
snapshot_files > "$work/files.after"
diff -u "$files_before" "$work/files.after" || fail "uninstall left files behind or removed others"
snapshot_claude > "$work/claude.after"
lost="$(comm -23 "$claude_before" "$work/claude.after")"
[ -z "$lost" ] || fail "uninstall removed: $lost"
extra="$(comm -13 "$claude_before" "$work/claude.after" | grep -v '/\.claude\.json\.bak\.' || true)"
[ -z "$extra" ] || fail "uninstall left: $extra"
end_step

echo "✓ install contract holds"
