#!/usr/bin/env bash
# Fails when the tool versions pinned in install.sh differ from the ones doctor checks against:
#   AST_INDEX_VERSION       <->  AST_INDEX_TESTED       crates/uals/src/doctor.rs
#   SEMBLE_VERSION          <->  SEMBLE_TESTED          crates/uals/src/doctor/semantic.rs
#   CLAUDE_CONTEXT_VERSION  <->  CLAUDE_CONTEXT_TESTED  (same file)
#   NODE_MIN                <->  NODE_MINIMUM           (same file)
# Usage: check-pins.sh [repo-dir]   (default: the repository this script is in)
set -euo pipefail

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
status=0

# shell_pin <name>: the value of NAME="a.b.c" in install.sh
shell_pin() {
  sed -n "s/^$1=\"\([0-9][0-9.]*\)\".*/\1/p" "$root/install.sh" | head -n1
}

# rust_pin <file> <name>: "a.b.c" from `const NAME: Version = Version::new(a, b, c);` or `Version([a, b, c])`
rust_pin() {
  sed -n "/const $2: Version = /{s/^.*= Version[^0-9]*//;s/[^0-9, ].*//;p;}" "$root/$1" | head -n1 | tr -d ' ' | tr ',' '.'
}

# check <install.sh name> <rust file> <rust name>
check() {
  local want have
  want="$(shell_pin "$1")"; have="$(rust_pin "$2" "$3")"
  if [ -z "$want" ] || [ -z "$have" ]; then
    echo "✗ cannot read $1 in install.sh or $3 in $2" >&2; status=1
  elif [ "$want" != "$have" ]; then
    echo "✗ $1=$want in install.sh, but $3=$have in $2" >&2; status=1
  else
    echo "✓ $1 = $3 = $want"
  fi
}

check AST_INDEX_VERSION crates/uals/src/doctor.rs AST_INDEX_TESTED
check SEMBLE_VERSION crates/uals/src/doctor/semantic.rs SEMBLE_TESTED
check CLAUDE_CONTEXT_VERSION crates/uals/src/doctor/semantic.rs CLAUDE_CONTEXT_TESTED
check NODE_MIN crates/uals/src/doctor/semantic.rs NODE_MINIMUM
exit "$status"
