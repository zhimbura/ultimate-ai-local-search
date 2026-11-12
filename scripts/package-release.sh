#!/usr/bin/env bash
# Packs a built uals binary the way a release archive is laid out (release.yml and CI use it).
#   package-release.sh <target> <binary> <tag> <out-dir>
# Writes <out-dir>/uals-<tag>-<target>.tar.gz (a folder uals-<tag>-<target>/ with uals, LICENSE and
# README.md) and <out-dir>/uals-<tag>-<target>.tar.gz.sha256.
set -euo pipefail

[ $# -eq 4 ] || { echo "usage: $0 <target> <binary> <tag> <out-dir>" >&2; exit 2; }
target="$1"; binary="$2"; tag="$3"; out_dir="$4"
[ -f "$binary" ] || { echo "✗ binary not found: $binary" >&2; exit 1; }

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
name="uals-${tag}-${target}"
mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir "$staging/$name"
cp "$binary" "$staging/$name/uals"
cp "$repo_dir/LICENSE" "$repo_dir/README.md" "$staging/$name/"

# COPYFILE_DISABLE keeps macOS tar from adding ._* metadata files.
COPYFILE_DISABLE=1 tar -C "$staging" -czf "$out_dir/$name.tar.gz" "$name"

cd "$out_dir"
if command -v sha256sum >/dev/null; then sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
else shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"; fi
echo "✓ $out_dir/$name.tar.gz"
