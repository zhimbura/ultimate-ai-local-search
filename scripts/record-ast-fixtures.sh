#!/usr/bin/env bash
# Regenerates the ast-index adapter fixtures from the PUBLIC test workspace.
#   tests/fixtures/workspace  →  crates/uals-adapters/tests/fixtures/ast_index/
# Every ast-index call runs with AST_INDEX_CACHE_DIR pointing at a throw-away dir,
# so the real cache (~/Library/Caches/ast-index) is never touched.
# Usage: scripts/record-ast-fixtures.sh [temp-root]   (default: a fresh mktemp -d)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/tests/fixtures/workspace"
OUT="$ROOT/crates/uals-adapters/tests/fixtures/ast_index"

command -v ast-index >/dev/null || { echo "✗ ast-index not installed" >&2; exit 1; }
command -v python3 >/dev/null || { echo "✗ python3 required (timing + sanitising)" >&2; exit 1; }

TMPBASE="${1:-$(mktemp -d "${TMPDIR:-/tmp}/uals-ast-fixtures.XXXXXX")}"
mkdir -p "$TMPBASE"
RUN="$(cd "$TMPBASE" && pwd -P)/s2-$(date +%Y%m%d-%H%M%S)"
WS="$RUN/ws"
export AST_INDEX_CACHE_DIR="$RUN/cache"
mkdir -p "$WS" "$AST_INDEX_CACHE_DIR" "$OUT"
export RUN WS
echo "▸ temp root: $RUN"
echo "▸ $(ast-index --version)"

cp -R "$SRC/." "$WS/"
rm -f "$OUT"/*.json "$OUT"/*.txt

# rec <name> <ext json|txt> <project-dir relative to $WS> <ast-index args...>
# Saves raw stdout (sanitised) + <name>.meta.json. Never fails the script on a non-zero exit.
rec() {
  local name="$1" ext="$2" proj="$3"; shift 3
  python3 - "$OUT" "$name" "$ext" "$proj" "$@" <<'PY'
import json, os, re, subprocess, sys, time
out, name, ext, proj, *args = sys.argv[1:]
run, ws = os.environ["RUN"], os.environ["WS"]
cwd = os.path.join(ws, proj) if proj not in ("", ".") else ws
def clean(s):
    s = s.replace(ws, "$WORKSPACE").replace(run, "$TMPROOT")
    return re.sub(r"/[0-9a-f]{14,16}/", "/<cache-hash>/", s)
t0 = time.perf_counter()
p = subprocess.run(["ast-index", *args], cwd=cwd, capture_output=True, text=True)
ms = round((time.perf_counter() - t0) * 1000)
open(f"{out}/{name}.{ext}", "w").write(clean(p.stdout))
meta = {"cmd": "ast-index", "args": args, "project": proj, "exit_code": p.returncode,
        "stderr": clean(p.stderr), "duration_ms": ms}
open(f"{out}/{name}.meta.json", "w").write(json.dumps(meta, indent=2) + "\n")
PY
  echo "✓ $name"
}

J="--format"

# ── 1. index every project ───────────────────────────────────────────────────
for p in sample-android sample-web sample-tools; do
  (cd "$WS/$p" && git init -q && ast-index rebuild >/dev/null 2>&1)
  echo "✓ rebuilt $p"
done

# ── 2. sample-android: JSON commands ─────────────────────────────────────────
A=sample-android
HS=network/src/main/kotlin/sample/network/HostSelector.kt
rec stats                      json $A stats $J json
rec symbol_hostselector        json $A symbol HostSelector $J json
rec symbol_fuzzy               json $A symbol hostsel --fuzzy $J json
rec symbol_pattern_store       json $A symbol -p '*Store' $J json
rec class_hostselector         json $A class HostSelector $J json
rec search_hostselector        json $A search HostSelector $J json
rec search_retry_backoff       json $A search "retry backoff" $J json
rec explore_retry_backoff      json $A explore retry with backoff $J json
rec explore_choose_backend_url json $A explore choose backend url $J json
rec usages_hostselector        json $A usages HostSelector $J json
rec usages_retrypolicy         json $A usages RetryPolicy $J json
rec refs_retrypolicy           json $A refs RetryPolicy $J json
rec implementations_hoststore  json $A implementations HostStore $J json
rec implementations_analytics  json $A implementations Analytics $J json
rec callers_markfailed         json $A callers markFailed $J json
rec outline_hostselector       json $A outline "$HS" $J json
rec file_hostselector          json $A file HostSelector $J json
rec module_route_app_core      json $A module-route --from app --to core $J json
rec module_route_core_app      json $A module-route --from core --to app $J json
rec db_path                    txt  $A db-path

# ── SQL via `query` ──────────────────────────────────────────────────────────
rec schema json $A schema
rec query_symbol_ranges json $A query \
  "SELECT f.path AS path, s.name, s.kind, s.line, s.end_line FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path IN ('$HS','core/src/main/kotlin/sample/core/RetryPolicy.kt','network/src/main/kotlin/sample/network/HostStore.kt') ORDER BY f.path, s.line" $J json
rec query_metadata json $A query "SELECT key, value FROM metadata ORDER BY key" $J json
rec query_module_deps_network json $A query \
  "SELECT m.name AS module, d.name AS depends_on, md.dep_kind FROM module_deps md JOIN modules m ON m.id = md.module_id JOIN modules d ON d.id = md.dep_module_id WHERE m.name = 'network' ORDER BY d.name" $J json
rec query_module_rdeps_core json $A query \
  "SELECT m.name AS module, d.name AS depends_on, md.dep_kind FROM module_deps md JOIN modules m ON m.id = md.module_id JOIN modules d ON d.id = md.dep_module_id WHERE d.name = 'core' ORDER BY m.name" $J json
rec query_modules json $A query "SELECT id, name, path, kind FROM modules ORDER BY name" $J json
rec query_files json $A query "SELECT id, path, root_path, mtime, size FROM files ORDER BY path" $J json
rec query_inheritance json $A query \
  "SELECT s.name AS child, i.parent_name, i.kind FROM inheritance i JOIN symbols s ON s.id = i.child_id ORDER BY s.name" $J json
rec query_error_non_select json $A query "DELETE FROM files" $J json

# ── text-only commands (they ignore --format json) ───────────────────────────
rec deps_network_text      txt $A deps network $J json
rec dependents_core_text   txt $A dependents core $J json
rec hierarchy_hoststore_text txt $A hierarchy HostStore $J json

# ── other projects ───────────────────────────────────────────────────────────
rec web_symbol_fetchwithretry json sample-web symbol fetchWithRetry $J json
rec web_outline_fetchwithretry json sample-web outline src/http/fetchWithRetry.ts $J json
rec web_explore_retry json sample-web explore retry with backoff $J json
rec tools_symbol_lrucache json sample-tools symbol LruCache $J json
rec tools_outline_cache json sample-tools outline sample_tools/cache.py $J json
rec tools_explore_cache json sample-tools explore lru cache eviction $J json

# ── root detection / probing ─────────────────────────────────────────────────
rec stats_from_subdir json "$A/network" stats $J json
rec symbol_from_subdir json "$A/network" symbol HostSelector $J json
rec probe_workspace_root_stats txt . stats $J json
echo "! probing a marker-less folder created stub cache dirs:"
for d in "$AST_INDEX_CACHE_DIR"/*/; do
  [[ -f "$d/index.db" ]] || echo "    stub: $(basename "$d") ($(ls -A "$d" | tr '\n' ' '))"
done

# ── 3. missing index ─────────────────────────────────────────────────────────
mkdir -p "$RUN/missing"
cp -R "$SRC/sample-android" "$RUN/missing/sample-android"
(cd "$RUN/missing/sample-android" && git init -q)
WS_SAVE="$WS"
export WS="$RUN/missing"
rec missing_index_stats txt sample-android stats $J json
rec missing_index_symbol txt sample-android symbol X $J json
rec missing_index_module_route txt sample-android module-route --from app --to core $J json
rec missing_index_query txt sample-android query "SELECT 1" $J json
export WS="$WS_SAVE"

# ── 4. staleness (separate copy so earlier fixtures stay clean) ──────────────
mkdir -p "$RUN/stale"
cp -R "$SRC/sample-android" "$RUN/stale/sample-android"
(cd "$RUN/stale/sample-android" && git init -q && ast-index rebuild >/dev/null 2>&1)
export WS="$RUN/stale"
S="$WS/sample-android"
rec stale_before_metadata json sample-android query "SELECT key, value FROM metadata ORDER BY key" $J json
rec stale_before_files json sample-android query "SELECT path, mtime, size FROM files ORDER BY path" $J json
sleep 1
printf '\n// edited\n' >> "$S/$HS"
printf 'package sample.network\n\nclass NewThing\n' > "$S/network/src/main/kotlin/sample/network/NewThing.kt"
printf '\nplugins { }\n' >> "$S/core/build.gradle.kts"
rec stale_after_edit_symbol_newthing json sample-android symbol NewThing $J json
rec stale_after_edit_outline json sample-android outline "$HS" $J json
rec stale_after_edit_module_route json sample-android module-route --from app --to core $J json
rec stale_after_edit_stats json sample-android stats $J json
rec stale_after_edit_metadata json sample-android query "SELECT key, value FROM metadata ORDER BY key" $J json
rec stale_after_edit_files json sample-android query "SELECT path, mtime, size FROM files ORDER BY path" $J json
rec stale_update txt sample-android update
rec stale_after_update_symbol_newthing json sample-android symbol NewThing $J json
rec stale_after_update_metadata json sample-android query "SELECT key, value FROM metadata ORDER BY key" $J json
rec stale_update_noop txt sample-android update
export WS="$WS_SAVE"

n_json=$(ls "$OUT"/*.json | grep -vc '\.meta\.json$' || true)
n_txt=$(ls "$OUT"/*.txt | wc -l | tr -d ' ')
echo "✓ done: $n_json json + $n_txt txt fixtures (+ meta) in ${OUT#"$ROOT"/}"
echo "! timestamps (metadata, files.mtime) and duration_ms differ between runs"
