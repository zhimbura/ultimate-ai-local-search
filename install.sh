#!/usr/bin/env bash
# uals (ultimate-ai-local-search): one code search for AI agents.
# Installs the uals binary, ast-index (symbols) and a semantic backend (Semble by default),
# writes the uals config and registers uals in the Claude Code config.
# macOS + Linux. Run from a checkout. Safe to re-run.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ─── pinned versions (the same ones are in 'uals doctor'; CI checks that they agree) ───
UALS_REPO="zhimbura/ultimate-ai-local-search"
AST_INDEX_VERSION="3.56.0"          # minimum; uals was tested with it
SEMBLE_SPEC="semble[mcp]==0.7.3"
SEMBLE_VERSION="0.7.3"
CLAUDE_CONTEXT_VERSION="0.1.16"
NODE_MIN="22.12.0"                  # a claude-context dependency uses require(ESM)
UV_VERSION="0.13.8"

# SHA-256 of the ast-index release assets (the release has no checksum files).
ast_index_sha256() {
  case "$1" in
    darwin-arm64)  echo 51f010c7a86f2049bd6347a0844ca6466c9b617e6962e2374f1f899e5cde23ba;;
    darwin-x86_64) echo 4355138d45edf200058a122a8746baaed5c08d76223fe42f4dff5b15294c6195;;
    linux-arm64)   echo 088e7f30ccd39629e9987e4d5e09d576e0c92190207fc4ab49f991a6b904edb0;;
    linux-x86_64)  echo 88c84ed45c16528c1884da0c960110aed42bc6697a5bba1604d954863bf6a1b8;;
    *) return 1;;
  esac
}

# ─── pretty output ───────────────────────────────────────────────────────────
if [ -t 1 ]; then B=$'\033[1m'; G=$'\033[32m'; Y=$'\033[33m'; R=$'\033[31m'; C=$'\033[36m'; N=$'\033[0m'; else B=; G=; Y=; R=; C=; N=; fi
say()  { printf '%s\n' "${C}▸${N} $*"; }
ok()   { printf '%s\n' "${G}✓${N} $*"; }
warn() { printf '%s\n' "${Y}!${N} $*" >&2; }
die()  { printf '%s\n' "${R}✗ $*${N}" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# shellcheck source=scripts/lib/env.sh
. "$REPO_DIR/scripts/lib/env.sh"

# ─── flags ───────────────────────────────────────────────────────────────────
usage() {
  cat <<EOF
Usage: ./install.sh [options]

Installs uals (one code search for AI agents) with ast-index and a semantic backend.
Safe to re-run: what is in place is left alone, and every file the installer rewrites
gets a backup first.

Options:
  --semantic <name>    semble | claude-context | none (default: semble)
  -y, --yes            never prompt; a missing cloud API key is an error
  --from-source        build uals from this checkout with cargo (--locked)
  --version <tag>      release to install (default: the latest GitHub release)
  --bin-dir <dir>      where binaries go (default: ~/.local/bin)
  --skip-ast-index     do not install ast-index
  --no-mcp             do not register uals in the Claude Code config
  --remove-claude-context
                       remove direct claude-context MCP entries from that config
                       (they are saved first; uninstall.sh puts them back)
  --no-doctor          do not run 'uals doctor' at the end
  --provider <name>    claude-context only: ollama | lmstudio | openai | voyageai | gemini | openrouter
  --model <name>       claude-context only: embedding model (default: per provider)
  --dimension <n>      claude-context only: embedding dimension, must match the model
  -h, --help           show this help

Environment:
  UALS_DOWNLOAD_BASE   release download base URL (file:// works; for tests)
  CLAUDE_CONFIG        Claude Code config to edit (default: ~/.claude.json)
  UALS_CONFIG          uals config to write (default: ${XDG_CONFIG_HOME:-~/.config}/uals/config.toml)
  OPENAI_API_KEY, VOYAGEAI_API_KEY, GEMINI_API_KEY, OPENROUTER_API_KEY
                       cloud API key for the chosen claude-context provider
  OLLAMA_HOST          Ollama URL (default http://127.0.0.1:11434)
  MILVUS_PORT, MILVUS_HEALTH_PORT, MILVUS_VERSION
                       Milvus settings, claude-context only (see .env.example)

Exit code: the exit code of 'uals doctor'; non-zero on an install error.
EOF
}

SEMANTIC=semble; ASSUME_YES=0; FROM_SOURCE=0; VERSION=""; BIN_DIR="$HOME/.local/bin"
DO_AST=1; DO_MCP=1; REMOVE_CC=0; DO_DOCTOR=1
PROVIDER=""; MODEL=""; DIMENSION=""
while [ $# -gt 0 ]; do case "$1" in
  --semantic|--version|--bin-dir|--provider|--model|--dimension)
    [ $# -ge 2 ] || die "$1 needs a value"
    case "$1" in
      --semantic) SEMANTIC="$2";; --version) VERSION="$2";; --bin-dir) BIN_DIR="$2";;
      --provider) PROVIDER="$2";; --model) MODEL="$2";; --dimension) DIMENSION="$2";;
    esac; shift 2;;
  --yes|-y) ASSUME_YES=1; shift;;
  --from-source) FROM_SOURCE=1; shift;;
  --skip-ast-index) DO_AST=0; shift;;
  --no-mcp) DO_MCP=0; shift;;
  --remove-claude-context) REMOVE_CC=1; shift;;
  --no-doctor) DO_DOCTOR=0; shift;;
  -h|--help) usage; exit 0;;
  *) die "unknown flag: $1";;
esac; done

case "$SEMANTIC" in semble|claude-context|none) ;; *) die "--semantic must be semble, claude-context or none";; esac
if [ "$REMOVE_CC" = 1 ] && [ "$DO_MCP" = 0 ]; then
  warn "--remove-claude-context has no effect with --no-mcp: the Claude Code config is not touched"
fi
if [ "$SEMANTIC" != claude-context ] && [ -n "$PROVIDER$MODEL$DIMENSION" ]; then
  warn "--provider/--model/--dimension only apply to --semantic claude-context: ignored"
fi
[ "$FROM_SOURCE" = 0 ] || [ -z "$VERSION" ] || die "--from-source and --version exclude each other"

OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS" in Darwin) PLATFORM=macos;; Linux) PLATFORM=linux;; *) die "unsupported OS: $OS (Windows is not supported)";; esac
case "$PLATFORM/$ARCH" in
  macos/arm64|macos/aarch64)  TARGET=aarch64-apple-darwin;      AST_ASSET=darwin-arm64;;
  macos/x86_64)               TARGET=x86_64-apple-darwin;       AST_ASSET=darwin-x86_64;;
  linux/x86_64|linux/amd64)   TARGET=x86_64-unknown-linux-musl; AST_ASSET=linux-x86_64;;
  linux/arm64|linux/aarch64)  TARGET=aarch64-unknown-linux-musl; AST_ASSET=linux-arm64;;
  *) die "unsupported platform: $PLATFORM/$ARCH";;
esac
say "${B}uals${N} installer, platform: $PLATFORM/$ARCH"

# Paths. They must be absolute: the config, the MCP entry and the manifest store them. A relative
# path means relative to the folder the script was started from.
abs_path() {
  local p="$1" d
  case "$p" in /*) ;; *) p="$PWD/$p";; esac
  d="$(dirname "$p")"
  if [ -d "$d" ]; then printf '%s/%s' "$(cd "$d" && pwd)" "$(basename "$p")"; else printf '%s' "$p"; fi
}
mkdir -p "$BIN_DIR" 2>/dev/null || true
[ -d "$BIN_DIR" ] || die "cannot use bin dir: $BIN_DIR"
BIN_DIR="$(cd "$BIN_DIR" && pwd)"
DATA_DIR="$HOME/.local/share/uals"
MANIFEST="$DATA_DIR/install-manifest"
CLAUDE_CONFIG="$(abs_path "${CLAUDE_CONFIG:-$HOME/.claude.json}")"
UALS_CONFIG_PATH="$(abs_path "${UALS_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/uals/config.toml}")"
CC_DIR="$DATA_DIR/claude-context"
SEMBLE_HOME="$DATA_DIR/semble"
DOWNLOAD_BASE="${UALS_DOWNLOAD_BASE:-https://github.com/$UALS_REPO/releases/download}"

TMP_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMP_ROOT"' EXIT

# ─── manifest: one line per thing THIS installer created or started ──────────
#   <kind> <value>   kinds: file, moved-aside, dir, emptydir, docker-stack, config-file,
#                    mcp-entry, saved-mcp
manifest_add() {
  local line="$1 $2"
  mkdir -p "$DATA_DIR"
  grep -qxF -- "$line" "$MANIFEST" 2>/dev/null || printf '%s\n' "$line" >> "$MANIFEST"
}
in_manifest() { grep -qxF -- "$1 $2" "$MANIFEST" 2>/dev/null; }

# ─── helpers ─────────────────────────────────────────────────────────────────
sha256_of() {
  if have shasum; then shasum -a 256 "$1" | awk '{print $1}'
  else sha256sum "$1" | awk '{print $1}'; fi
}
fetch() { curl -fsSL --retry 2 -o "$2" "$1"; }          # fetch <url> <dest>

# verify_sha256 <file> <want>: dies unless the SHA-256 of the file is <want>
verify_sha256() {
  local file="$1" want="$2" got
  got="$(sha256_of "$file")"
  [ -n "$want" ] && [ "$want" = "$got" ] || die "SHA-256 mismatch for ${file##*/} (want ${want:-none}, got $got)"
}

# fetch_archive <archive-url> <dest-dir>: downloads <archive-url> and <archive-url>.sha256 into
# <dest-dir>, checks the archive against the checksum file and unpacks it there.
fetch_archive() {
  local url="$1" dir="$2" archive="${1##*/}"
  mkdir -p "$dir"
  fetch "$url" "$dir/$archive" || die "cannot download $url"
  fetch "$url.sha256" "$dir/$archive.sha256" || die "cannot download $url.sha256"
  verify_sha256 "$dir/$archive" "$(awk '{print $1; exit}' "$dir/$archive.sha256")"
  tar -xzf "$dir/$archive" -C "$dir"
}
# first_version: reads text on stdin, prints the first dotted number in it
first_version() { sed -n 's/[^0-9]*\([0-9][0-9]*\(\.[0-9][0-9]*\)*\).*/\1/p' | head -n1; }
ver_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]; }   # ver_ge <have> <min>
toml_str() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }

# install_bin <src> <dst> [manifest-kind]: put an executable in place, atomically.
# Unchanged: returns 1 and changes nothing. A file that is not ours is moved aside first, and
# uninstall.sh puts it back (manifest kind moved-aside).
install_bin() {
  local src="$1" dst="$2" kind="${3:-file}"
  if [ -e "$dst" ] && cmp -s "$src" "$dst"; then return 1; fi
  if [ -e "$dst" ] && ! in_manifest "$kind" "$dst" && ! in_manifest file "$dst"; then
    local bak; bak="$dst.bak.$(date +%Y%m%d-%H%M%S)"
    mv -f "$dst" "$bak" || die "cannot move $dst aside"
    manifest_add moved-aside "$bak"
    warn "$dst was not installed by uals: moved to $bak (uninstall.sh puts it back)"
  fi
  { install -m755 "$src" "$dst.new.$$" && mv -f "$dst.new.$$" "$dst"; } || die "cannot install $dst"
  manifest_add "$kind" "$dst"
}

SUDO=""
ensure_pkg() { # ensure_pkg <cmd> <brew-formula> [apt-pkg]
  local cmd="$1" brew_f="$2" apt_p="${3:-$1}"
  have "$cmd" && return 0
  say "installing $cmd…"
  if have brew; then brew install "$brew_f"
  elif [ "$PLATFORM" = linux ] && have apt-get; then
    if [ "$(id -u)" != 0 ]; then
      have sudo || die "need root or sudo to install $cmd"
      SUDO=sudo
    fi
    $SUDO apt-get update -qq && $SUDO apt-get install -y "$apt_p"
  else die "cannot install '$cmd' automatically: install it and re-run"; fi
}

# ─── prerequisites ───────────────────────────────────────────────────────────
ensure_pkg curl curl
have tar || die "tar not found"
have shasum || have sha256sum || die "no SHA-256 tool (shasum or sha256sum)"
ensure_pkg jq jq
ok "curl, tar, SHA-256 tool and jq ready"

# ─── uals ────────────────────────────────────────────────────────────────────
UALS_BIN="$BIN_DIR/uals"
install_uals() {
  if [ "$FROM_SOURCE" = 1 ]; then
    have cargo || die "--from-source needs cargo (install Rust: https://rustup.rs)"
    [ -f "$REPO_DIR/Cargo.toml" ] || die "no Cargo.toml in $REPO_DIR: run install.sh from a checkout"
    say "building uals from source (cargo build --release --locked)…"
    (cd "$REPO_DIR" && cargo build --release --locked -p uals)
    local built="$REPO_DIR/target/release/uals"
    [ -x "$built" ] || die "cargo did not produce $built"
    if install_bin "$built" "$UALS_BIN"; then ok "uals → $UALS_BIN (built from source)"
    else ok "uals already up to date ($UALS_BIN)"; fi
    return
  fi

  local tag="$VERSION"
  if [ -z "$tag" ]; then
    say "looking up the latest uals release…"
    tag="$(curl -fsSL -H 'Accept: application/vnd.github+json' "https://api.github.com/repos/$UALS_REPO/releases/latest" 2>/dev/null | jq -r '.tag_name // empty' 2>/dev/null || true)"
    [ -n "$tag" ] || die "no uals release found on GitHub: use --from-source (needs cargo) or --version <tag>"
  fi
  case "$tag" in v*) ;; *) tag="v$tag";; esac
  if [ -x "$UALS_BIN" ] && [ "$("$UALS_BIN" --version 2>/dev/null | awk '{print $2; exit}')" = "${tag#v}" ]; then
    ok "uals ${tag#v} already installed ($UALS_BIN)"; return
  fi

  local name="uals-$tag-$TARGET" dl="$TMP_ROOT/uals"
  say "downloading $name…"
  fetch_archive "$DOWNLOAD_BASE/$tag/$name.tar.gz" "$dl"
  [ -f "$dl/$name/uals" ] || die "the archive has no $name/uals"
  if install_bin "$dl/$name/uals" "$UALS_BIN"; then ok "uals $tag → $UALS_BIN"
  else ok "uals already up to date ($UALS_BIN)"; fi
}
install_uals

# ─── ast-index ───────────────────────────────────────────────────────────────
AST_BIN=""
ast_ok() { [ -x "$1" ] && ver_ge "$("$1" --version 2>/dev/null | first_version)" "$AST_INDEX_VERSION"; }

install_ast_index() {
  say "installing ast-index $AST_INDEX_VERSION ($AST_ASSET)…"
  local dl="$TMP_ROOT/ast" asset="ast-index-v$AST_INDEX_VERSION-$AST_ASSET.tar.gz"
  mkdir -p "$dl"
  fetch "https://github.com/defendend/Claude-ast-index-search/releases/download/v$AST_INDEX_VERSION/$asset" "$dl/$asset" || die "cannot download $asset"
  verify_sha256 "$dl/$asset" "$(ast_index_sha256 "$AST_ASSET")"
  tar -xzf "$dl/$asset" -C "$dl"
  [ -f "$dl/ast-index" ] || die "the archive has no ast-index"
  install_bin "$dl/ast-index" "$BIN_DIR/ast-index" || true
  AST_BIN="$BIN_DIR/ast-index"
  ok "ast-index $AST_INDEX_VERSION → $AST_BIN"
}

if [ "$DO_AST" = 1 ]; then
  if ast_ok "$BIN_DIR/ast-index"; then AST_BIN="$BIN_DIR/ast-index"
  elif have ast-index && ast_ok "$(command -v ast-index)"; then AST_BIN="$(command -v ast-index)"
  fi
  if [ -n "$AST_BIN" ]; then ok "ast-index present ($AST_BIN)"; else install_ast_index; fi
fi

# ─── semantic backend: semble ────────────────────────────────────────────────
# Semble lives in its own folder ($SEMBLE_HOME: uv tools, bin, managed Python, and uv itself when
# it has to be downloaded), so uninstall.sh removes it with one folder and touches nothing of the
# user's uv.
SEMBLE_BIN=""
semble_is_pinned() { [ -x "$1" ] && [ "$("$1" --version 2>/dev/null | first_version)" = "$SEMBLE_VERSION" ]; }

install_semble() {
  if have semble && semble_is_pinned "$(command -v semble)"; then
    SEMBLE_BIN="$(command -v semble)"
    ok "semble $SEMBLE_VERSION found on PATH ($SEMBLE_BIN): used as it is"
    return
  fi
  if semble_is_pinned "$SEMBLE_HOME/bin/semble"; then
    SEMBLE_BIN="$SEMBLE_HOME/bin/semble"
    ok "semble $SEMBLE_VERSION already installed ($SEMBLE_BIN)"
    return
  fi
  [ -d "$SEMBLE_HOME" ] || manifest_add dir "$SEMBLE_HOME"
  mkdir -p "$SEMBLE_HOME"

  local uv_bin=""
  if have uv; then
    uv_bin="$(command -v uv)"
  else
    say "downloading uv $UV_VERSION…"
    local name="uv-$TARGET" dl="$TMP_ROOT/uv"
    fetch_archive "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/$name.tar.gz" "$dl"
    [ -f "$dl/$name/uv" ] || die "the archive has no $name/uv"
    mkdir -p "$SEMBLE_HOME/uv"
    install -m755 "$dl/$name/uv" "$SEMBLE_HOME/uv/uv" || die "cannot install uv into $SEMBLE_HOME/uv"
    uv_bin="$SEMBLE_HOME/uv/uv"
    ok "uv $UV_VERSION → $uv_bin"
  fi

  say "installing $SEMBLE_SPEC…"
  local log="$TMP_ROOT/uv-tool.log"
  UV_TOOL_DIR="$SEMBLE_HOME/tools" UV_TOOL_BIN_DIR="$SEMBLE_HOME/bin" \
    UV_PYTHON_INSTALL_DIR="$SEMBLE_HOME/python" UV_PYTHON_BIN_DIR="$SEMBLE_HOME/python-bin" \
    UV_CACHE_DIR="$TMP_ROOT/uv-cache" UV_LINK_MODE=copy \
    "$uv_bin" tool install --force --python ">=3.10" "$SEMBLE_SPEC" >"$log" 2>&1 || { cat "$log" >&2; die "cannot install $SEMBLE_SPEC"; }
  SEMBLE_BIN="$SEMBLE_HOME/bin/semble"
  ok "semble $SEMBLE_VERSION → $SEMBLE_BIN"
}
[ "$SEMANTIC" != semble ] || install_semble

# ─── semantic backend: claude-context (Milvus + embedder + npm package) ─────
EMB_PROVIDER=""; OPENAI_BASE_URL=""; OLLAMA_HOST="${OLLAMA_HOST:-http://127.0.0.1:11434}"
API_KEY_VAR=""; API_KEY_VAL=""; NODE_BIN=""
ENV_FILE="$REPO_DIR/.env"
OLLAMA_WAIT_SECONDS=30
MILVUS_WAIT_TRIES=60     # 3 s apart

select_provider() {
  local env_provider="" env_base_url=""
  if [ -f "$ENV_FILE" ]; then
    say "found existing .env: using it for defaults (flags override)"
    env_load_file "$ENV_FILE" defaults
    case "${EMBEDDING_PROVIDER:-}" in
      Ollama) env_provider=ollama;;
      OpenAI) case "${OPENAI_BASE_URL:-}" in *:1234*) env_provider=lmstudio;; *) env_provider=openai;; esac;;
      VoyageAI) env_provider=voyageai;;
      Gemini) env_provider=gemini;;
      OpenRouter) env_provider=openrouter;;
    esac
    [ "$env_provider" != openai ] || env_base_url="${OPENAI_BASE_URL:-}"   # a local OpenAI-compatible server
    [ -n "$PROVIDER" ] || PROVIDER="$env_provider"
    # model/dimension in .env belong to its provider: reuse only when it is the one chosen
    if [ -n "$PROVIDER" ] && [ "$PROVIDER" = "$env_provider" ]; then
      MODEL="${MODEL:-${EMBEDDING_MODEL:-}}"; DIMENSION="${DIMENSION:-${EMBEDDING_DIMENSION:-}}"
    fi
  fi
  MILVUS_PORT="${MILVUS_PORT:-19530}"

  if [ -z "$PROVIDER" ] && [ "$ASSUME_YES" = 1 ]; then
    PROVIDER=ollama
    say "--yes: no provider given, defaulting to ollama"
  fi
  if [ -z "$PROVIDER" ]; then
    cat <<EOF

Where should embeddings be computed?
  ${B}Local${N} (private, free, offline):
    1) Ollama       headless, simplest
    2) LM Studio    via lms CLI + REST :1234
  ${B}Cloud${N} (needs an API key):
    3) OpenAI       text-embedding-3-small/large
    4) VoyageAI     voyage-code-3 (best for code)
    5) Gemini       text-embedding-004
    6) OpenRouter   proxy to many providers
EOF
    read -rp "Choose [1-6] (default 1): " choice
    case "${choice:-1}" in
      1) PROVIDER=ollama;; 2) PROVIDER=lmstudio;; 3) PROVIDER=openai;;
      4) PROVIDER=voyageai;; 5) PROVIDER=gemini;; 6) PROVIDER=openrouter;;
      *) die "invalid choice";;
    esac
  fi

  # The address from .env belongs to the provider it was set for: it stays only for openai
  # when .env says openai too (a custom server), and is dropped for every other provider.
  OPENAI_BASE_URL=""
  case "$PROVIDER" in
    ollama)     EMB_PROVIDER=Ollama;  MODEL="${MODEL:-nomic-embed-text}";               DIMENSION="${DIMENSION:-768}";;
    lmstudio)   EMB_PROVIDER=OpenAI;  MODEL="${MODEL:-text-embedding-nomic-embed-text-v1.5}"; DIMENSION="${DIMENSION:-768}"; OPENAI_BASE_URL="http://127.0.0.1:1234/v1";;
    openai)     EMB_PROVIDER=OpenAI;  MODEL="${MODEL:-text-embedding-3-small}";          DIMENSION="${DIMENSION:-1536}"; OPENAI_BASE_URL="$env_base_url";;
    voyageai)   EMB_PROVIDER=VoyageAI;MODEL="${MODEL:-voyage-code-3}";                   DIMENSION="${DIMENSION:-1024}";;
    gemini)     EMB_PROVIDER=Gemini;  MODEL="${MODEL:-text-embedding-004}";              DIMENSION="${DIMENSION:-768}";;
    openrouter) EMB_PROVIDER=OpenRouter; MODEL="${MODEL:-openai/text-embedding-3-small}";DIMENSION="${DIMENSION:-1536}";;
    *) die "unknown provider: $PROVIDER";;
  esac
  say "provider=${B}$PROVIDER${N} model=${B}$MODEL${N} dim=${B}$DIMENSION${N}${OPENAI_BASE_URL:+ url=${B}$OPENAI_BASE_URL${N}}"
}

setup_provider() {
  case "$PROVIDER" in
    ollama)
      ollama_up() { curl -fsS --noproxy '*' "$OLLAMA_HOST/api/tags" >/dev/null 2>&1; }
      if ! have ollama; then
        say "installing Ollama…"
        if [ "$PLATFORM" = macos ] && have brew; then brew install ollama
        else curl -fsSL https://ollama.com/install.sh | sh; fi
      fi
      if have ollama && ! ollama_up; then
        say "starting Ollama…"
        if [ "$PLATFORM" = macos ] && have brew; then brew services start ollama >/dev/null 2>&1 || ( ollama serve >/dev/null 2>&1 & )
        else ( ollama serve >/dev/null 2>&1 & ); fi
        for _ in $(seq 1 "$OLLAMA_WAIT_SECONDS"); do ollama_up && break; sleep 1; done
        ollama_up || die "Ollama API did not come up at $OLLAMA_HOST within ${OLLAMA_WAIT_SECONDS}s: start it ('ollama serve') and re-run"
      fi
      say "pulling model $MODEL…"; ollama pull "$MODEL"
      ;;
    lmstudio)
      local lms="$HOME/.lmstudio/bin/lms"
      have lms && lms=lms
      if [ ! -x "$lms" ] && ! have lms; then
        if [ "$PLATFORM" = macos ] && have brew; then brew install --cask lm-studio || die "install LM Studio manually: https://lmstudio.ai"
        else die "LM Studio: install it from https://lmstudio.ai, then re-run (the 'lms' CLI is needed)"; fi
        lms="$HOME/.lmstudio/bin/lms"
      fi
      say "downloading model + starting LM Studio server…"
      "$lms" get "$MODEL" 2>/dev/null || warn "could not auto-download '$MODEL': load it in LM Studio"
      "$lms" server start 2>/dev/null || warn "could not start LM Studio server: start it (port 1234)"
      "$lms" load "$MODEL" --yes 2>/dev/null || warn "could not load '$MODEL': load it in LM Studio, else the server reports 'No models loaded'"
      ;;
    openai)     API_KEY_VAR=OPENAI_API_KEY;     API_KEY_VAL="${OPENAI_API_KEY:-}"
                [ "$API_KEY_VAL" = lm-studio ] && API_KEY_VAL="";;   # placeholder left by an LM Studio .env
    voyageai)   API_KEY_VAR=VOYAGEAI_API_KEY;   API_KEY_VAL="${VOYAGEAI_API_KEY:-}";;
    gemini)     API_KEY_VAR=GEMINI_API_KEY;     API_KEY_VAL="${GEMINI_API_KEY:-}";;
    openrouter) API_KEY_VAR=OPENROUTER_API_KEY; API_KEY_VAL="${OPENROUTER_API_KEY:-}";;
  esac
  if [ -n "$API_KEY_VAR" ] && [ -z "$API_KEY_VAL" ]; then
    [ "$ASSUME_YES" = 1 ] && die "$API_KEY_VAR is required for $PROVIDER (--yes disables the prompt): export it or put it in .env"
    read -rsp "Enter $API_KEY_VAR: " API_KEY_VAL; echo
    [ -n "$API_KEY_VAL" ] || die "$API_KEY_VAR is required for $PROVIDER"
  fi
  return 0
}

write_env_file() {
  local new="$TMP_ROOT/env.new"
  {
    echo "MILVUS_PORT=$MILVUS_PORT"
    echo "MILVUS_HEALTH_PORT=${MILVUS_HEALTH_PORT:-9091}"
    echo "MILVUS_VERSION=${MILVUS_VERSION:-v2.6.27}"
    echo "MILVUS_ADDRESS=127.0.0.1:$MILVUS_PORT"
    echo "EMBEDDING_PROVIDER=$EMB_PROVIDER"
    echo "EMBEDDING_MODEL=$MODEL"
    echo "EMBEDDING_DIMENSION=$DIMENSION"
    [ -z "$OPENAI_BASE_URL" ] || echo "OPENAI_BASE_URL=$OPENAI_BASE_URL"
    [ "$PROVIDER" != lmstudio ] || echo "OPENAI_API_KEY=lm-studio"
    [ "$PROVIDER" != ollama ] || echo "OLLAMA_HOST=$OLLAMA_HOST"
    [ -z "$API_KEY_VAR" ] || echo "$API_KEY_VAR=$API_KEY_VAL"
  } > "$new"
  if [ -f "$ENV_FILE" ] && cmp -s "$new" "$ENV_FILE"; then ok ".env already up to date"; return; fi
  if [ -f "$ENV_FILE" ]; then
    local bak; bak="$ENV_FILE.bak.$(date +%Y%m%d-%H%M%S)"
    while [ -e "$bak" ]; do sleep 1; bak="$ENV_FILE.bak.$(date +%Y%m%d-%H%M%S)"; done
    if in_manifest file "$ENV_FILE"; then
      cp -p "$ENV_FILE" "$bak"; chmod 600 "$bak"
      ok "previous .env backed up to ${bak##*/}"
    else
      mv -f "$ENV_FILE" "$bak" || die "cannot move $ENV_FILE aside"
      chmod 600 "$bak"
      manifest_add moved-aside "$bak"
      warn "$ENV_FILE was not written by uals: moved to ${bak##*/} (uninstall.sh puts it back)"
    fi
  fi
  ( umask 077; cat "$new" > "$ENV_FILE" )
  chmod 600 "$ENV_FILE"
  manifest_add file "$ENV_FILE"
  ok ".env written (chmod 600, gitignored)"
}

start_milvus() {
  have docker || die "Docker not found. Install Docker Desktop / OrbStack / Docker Engine, then re-run."
  docker info >/dev/null 2>&1 || die "Docker is installed but not running. Start it and re-run."
  docker compose version >/dev/null 2>&1 || die "Docker Compose v2 required (docker compose ...)."
  local health_port="${MILVUS_HEALTH_PORT:-9091}"
  local state
  state="$(docker inspect -f '{{.State.Running}}' uals-milvus 2>/dev/null || true)"
  if [ "$state" = true ]; then
    ok "Milvus container uals-milvus is already running: left as it is"
  elif [ "$state" = false ]; then
    say "starting the existing uals-milvus container…"
    docker start uals-milvus >/dev/null
  else
    say "starting Milvus (one container, embedded etcd)…"
    manifest_add docker-stack "$REPO_DIR"
    (cd "$REPO_DIR" && docker compose --env-file "$ENV_FILE" up -d)
  fi
  say "waiting for Milvus to become healthy (first run can take ~1-2 min)…"
  local i
  for i in $(seq 1 "$MILVUS_WAIT_TRIES"); do
    if curl -fsS --noproxy '*' "http://127.0.0.1:${health_port}/healthz" >/dev/null 2>&1; then ok "Milvus healthy"; return; fi
    [ "$i" = "$MILVUS_WAIT_TRIES" ] && die "Milvus did not become healthy. Check: docker compose logs milvus"
    sleep 3
  done
}

install_claude_context() {
  have node && have npm || die "claude-context needs Node.js >= $NODE_MIN and npm"
  NODE_BIN="$(command -v node)"
  ver_ge "$("$NODE_BIN" --version | first_version)" "$NODE_MIN" || die "Node $("$NODE_BIN" --version) is too old: claude-context needs >= $NODE_MIN (require(ESM))"
  select_provider
  setup_provider
  write_env_file
  start_milvus
  local pkg="$CC_DIR/node_modules/@zilliz/claude-context-mcp/package.json"
  if [ -f "$pkg" ] && [ "$(jq -r .version "$pkg")" = "$CLAUDE_CONTEXT_VERSION" ]; then
    ok "claude-context $CLAUDE_CONTEXT_VERSION already installed"
  else
    say "installing @zilliz/claude-context-mcp@$CLAUDE_CONTEXT_VERSION into $CC_DIR…"
    [ -d "$CC_DIR" ] || manifest_add dir "$CC_DIR"
    mkdir -p "$CC_DIR"
    npm install --prefix "$CC_DIR" --no-audit --no-fund --loglevel=error "@zilliz/claude-context-mcp@$CLAUDE_CONTEXT_VERSION"
    ok "claude-context $CLAUDE_CONTEXT_VERSION → $CC_DIR"
  fi
}
[ "$SEMANTIC" != claude-context ] || install_claude_context

# ─── uals config ─────────────────────────────────────────────────────────────
# Absolute tool paths: Claude Code may start `uals mcp` with a short PATH.
loopback_url() { case "$1" in http://127.0.0.1|http://127.0.0.1[:/]*|https://127.0.0.1|https://127.0.0.1[:/]*) return 0;; *) return 1;; esac; }

render_config() {
  echo "# Written by install.sh. Tool paths are absolute because Claude Code may start uals with a short PATH."
  echo "# Never put API keys here: list the variable names in pass_env."
  if [ -n "$AST_BIN" ]; then
    printf '\n[ast_index]\nbinary = %s\n' "$(toml_str "$AST_BIN")"
  fi
  printf '\n[semantic]\ndefault = %s\n' "$(toml_str "$SEMANTIC")"
  case "$SEMANTIC" in
    semble)
      printf '\n[semantic.semble]\ncommand = [%s]\n' "$(toml_str "$SEMBLE_BIN")";;
    claude-context)
      local env_items="" pass_items="" kv
      add_env() { kv="$1 = $(toml_str "$2")"; env_items="${env_items:+$env_items, }$kv"; }
      add_pass() { pass_items="${pass_items:+$pass_items, }$(toml_str "$1")"; }
      add_env MILVUS_ADDRESS "127.0.0.1:$MILVUS_PORT"
      add_env EMBEDDING_PROVIDER "$EMB_PROVIDER"
      add_env EMBEDDING_MODEL "$MODEL"
      add_env EMBEDDING_DIMENSION "$DIMENSION"
      if [ "$PROVIDER" = ollama ]; then
        if loopback_url "$OLLAMA_HOST"; then add_env OLLAMA_HOST "$OLLAMA_HOST"
        else add_pass OLLAMA_HOST; warn "OLLAMA_HOST is not a 127.0.0.1 address: left out of config.toml, passed by name"; fi
      fi
      if [ -n "$OPENAI_BASE_URL" ]; then
        if loopback_url "$OPENAI_BASE_URL"; then add_env OPENAI_BASE_URL "$OPENAI_BASE_URL"
        else add_pass OPENAI_BASE_URL; fi
      fi
      if [ "$PROVIDER" = lmstudio ]; then add_env OPENAI_API_KEY lm-studio   # placeholder, not a secret
      elif [ -n "$API_KEY_VAR" ]; then add_pass "$API_KEY_VAR"; fi
      printf '\n[semantic.claude_context]\nnode = %s\nenv = { %s }\n' "$(toml_str "$NODE_BIN")" "$env_items"
      [ -z "$pass_items" ] || printf 'pass_env = [%s]\n' "$pass_items"
      ;;
  esac
}

toml_get() { # toml_get <file> <section> <key>: the raw value of key, empty when missing
  awk -v sec="$2" -v key="$3" '
    /^[ \t]*\[/ { cur = $0; gsub(/[][ \t]/, "", cur); next }
    cur == sec { k = $0; sub(/[ \t]*=.*/, "", k); gsub(/^[ \t]+/, "", k)
      if (k == key) { v = $0; sub(/^[^=]*=[ \t]*/, "", v); sub(/[ \t]+$/, "", v); print v; exit } }' "$1"
}
note_diff() { # note_diff <label> <existing raw> <wanted raw>
  if [ "$2" != "$3" ]; then warn "  $1: config has ${2:-(default)}, the chosen options give $3"; fi
}

OLD_BACKEND=""
# add_emptydirs <dir>: records every missing folder on the way to <dir> (outermost first) before it
# is created, so uninstall.sh removes only the folders the install made, and only when empty.
add_emptydirs() {
  local d="$1" missing=() i
  while [ ! -d "$d" ] && [ "$d" != / ]; do missing+=("$d"); d="$(dirname "$d")"; done
  for ((i = ${#missing[@]} - 1; i >= 0; i--)); do manifest_add emptydir "${missing[$i]}"; done
}

write_or_compare_config() {
  if [ -f "$UALS_CONFIG_PATH" ]; then
    ok "uals config exists: left as it is ($UALS_CONFIG_PATH)"
    if [ -n "$AST_BIN" ]; then note_diff "ast_index.binary" "$(toml_get "$UALS_CONFIG_PATH" ast_index binary)" "$(toml_str "$AST_BIN")"; fi
    local have_sem; have_sem="$(toml_get "$UALS_CONFIG_PATH" semantic default)"
    if [ "$have_sem" != "$(toml_str "$SEMANTIC")" ]; then
      OLD_BACKEND="${have_sem:-(default)}"
      warn "uals keeps using the old semantic backend ($OLD_BACKEND), not $SEMANTIC: set semantic.default = $(toml_str "$SEMANTIC") in $UALS_CONFIG_PATH, or remove that file and run the installer again"
    fi
    if [ "$SEMANTIC" = semble ]; then note_diff "semantic.semble.command" "$(toml_get "$UALS_CONFIG_PATH" semantic.semble command)" "[$(toml_str "$SEMBLE_BIN")]"; fi
    return
  fi
  add_emptydirs "$(dirname "$UALS_CONFIG_PATH")"
  mkdir -p "$(dirname "$UALS_CONFIG_PATH")"
  render_config > "$UALS_CONFIG_PATH"
  manifest_add config-file "$UALS_CONFIG_PATH"
  ok "uals config written ($UALS_CONFIG_PATH)"
}
write_or_compare_config

# ─── MCP registration ────────────────────────────────────────────────────────
mcp_entry() { [ -f "$CLAUDE_CONFIG" ] && jq -cS '.mcpServers.uals // empty' "$CLAUDE_CONFIG" 2>/dev/null; true; }
saved_mcp() { [ ! -f "$DATA_DIR/removed-mcp/claude-context.json" ] || cat "$DATA_DIR/removed-mcp/claude-context.json"; }
if [ "$DO_MCP" = 1 ]; then
  entry_before="$(mcp_entry)"; saved_before="$(saved_mcp)"
  mcp_args=(--uals-bin "$UALS_BIN" --config "$CLAUDE_CONFIG")
  # An entry that an earlier run of this installer wrote is ours: uninstall must not put it back.
  if in_manifest mcp-entry "$CLAUDE_CONFIG"; then mcp_args+=(--no-save-previous); fi
  [ "$SEMANTIC" != claude-context ] || [ ! -f "$ENV_FILE" ] || mcp_args+=(--env-file "$ENV_FILE")
  [ "$REMOVE_CC" = 0 ] || mcp_args+=(--remove-claude-context)
  bash "$REPO_DIR/scripts/configure-mcp.sh" "${mcp_args[@]}"
  [ "$(mcp_entry)" = "$entry_before" ] || manifest_add mcp-entry "$CLAUDE_CONFIG"
  [ "$(saved_mcp)" = "$saved_before" ] || manifest_add saved-mcp "$CLAUDE_CONFIG"
fi

# ─── doctor ──────────────────────────────────────────────────────────────────
rc=0
if [ "$DO_DOCTOR" = 1 ]; then
  say "running uals doctor…"
  "$UALS_BIN" doctor || rc=$?
fi

printf '\n%s\n' "${G}${B}Done.${N}"
echo "Next steps:"
step=1
echo "  $step) ${B}Restart your AI agent${N} (Claude Code / etc.) so it loads the uals MCP server."
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) step=$((step + 1))
     echo "  $step) $BIN_DIR is not on your PATH. To use the 'uals' command, add this line to your shell profile:"
     echo "       export PATH=\"$BIN_DIR:\$PATH\"";;
esac
if [ -n "$OLD_BACKEND" ]; then
  step=$((step + 1))
  echo "  $step) uals still uses the old semantic backend ($OLD_BACKEND), not $SEMANTIC: set semantic.default = $(toml_str "$SEMANTIC") in $UALS_CONFIG_PATH, or remove that file and run the installer again."
fi
echo "Rules for agents: see ${B}$REPO_DIR/rules/${N}: copy the snippets into your project's AGENTS.md / CLAUDE.md."
echo "Check the setup any time: ${B}$UALS_BIN doctor${N} · uninstall: ${B}$REPO_DIR/uninstall.sh${N}"
exit "$rc"
