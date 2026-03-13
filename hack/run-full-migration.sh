#!/usr/bin/env bash
#
# Full PF v5→v6 migration pipeline against quipucords-ui.
#
# Steps:
#   1. Clone quipucords-ui (or use existing) and reset to v5 commit
#   2. Build the provider
#   3. Start the gRPC provider
#   4. Run kantra analysis (frontend + builtin providers)
#   5. Convert output to JSON
#   6. Apply pattern-based fixes
#   7. Apply goose LLM fixes (optional, requires --with-goose)
#   8. Re-analyze to measure improvement
#   9. Compare against real v6 migration
#
# Usage:
#   ./hack/run-full-migration.sh                        # pattern fixes only
#   ./hack/run-full-migration.sh --with-goose           # pattern + goose LLM fixes
#   ./hack/run-full-migration.sh --skip-build           # skip cargo build
#   ./hack/run-full-migration.sh --skip-kantra          # use standalone analyze instead of kantra
#   ./hack/run-full-migration.sh --include-testing-rules # include DOM/CSS/a11y/behavioral proxy rules

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORK_DIR="/tmp/quipucords-migration-test"
REPO_URL="git@github.com:jwmatthews/quipucords-ui.git"
V5_COMMIT="3b3ce52"
PROVIDER_PORT=9001
PROVIDER_PID=""

# Parse flags
WITH_GOOSE=false
SKIP_BUILD=false
SKIP_KANTRA=false
INCLUDE_TESTING_RULES=false
LOG_DIR=""
for arg in "$@"; do
  case $arg in
    --with-goose) WITH_GOOSE=true ;;
    --skip-build) SKIP_BUILD=true ;;
    --skip-kantra) SKIP_KANTRA=true ;;
    --include-testing-rules) INCLUDE_TESTING_RULES=true ;;
    --log-dir=*) LOG_DIR="${arg#*=}" ;;
    *) echo "Unknown flag: $arg"; exit 1 ;;
  esac
done

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
fail()  { echo -e "${RED}[FAIL]${NC}  $*"; }

cleanup() {
  if [ -n "$PROVIDER_PID" ] && kill -0 "$PROVIDER_PID" 2>/dev/null; then
    info "Stopping provider (PID $PROVIDER_PID)"
    kill "$PROVIDER_PID" 2>/dev/null || true
    wait "$PROVIDER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# ── Step 0: Setup ──────────────────────────────────────────────────────────

info "Working directory: $WORK_DIR"
mkdir -p "$WORK_DIR"

BINARY="$PROJECT_DIR/target/release/frontend-analyzer-provider"
RULES_DIR="$PROJECT_DIR/rules/patternfly-v5-to-v6"
PROVIDER_SETTINGS="$PROJECT_DIR/provider_settings.json"
QUIPUCORDS="$WORK_DIR/quipucords-ui"
OUTPUT_DIR="$WORK_DIR/analysis-output"
ANALYSIS_JSON="$WORK_DIR/analysis.json"

# ── Step 1: Clone / Reset quipucords-ui ────────────────────────────────────

info "Step 1: Preparing quipucords-ui at v5 commit ($V5_COMMIT)"

if [ -d "$QUIPUCORDS/.git" ]; then
  info "  Repository exists, resetting to $V5_COMMIT"
  git -C "$QUIPUCORDS" checkout "$V5_COMMIT" -- . 2>/dev/null
else
  info "  Cloning from $REPO_URL"
  git clone --quiet "$REPO_URL" "$QUIPUCORDS"
  git -C "$QUIPUCORDS" checkout "$V5_COMMIT" -- . 2>/dev/null
fi
ok "quipucords-ui ready at $(git -C "$QUIPUCORDS" log --oneline -1 2>/dev/null)"

# ── Step 2: Build provider ────────────────────────────────────────────────

if [ "$SKIP_BUILD" = false ]; then
  info "Step 2: Building frontend-analyzer-provider"
  (cd "$PROJECT_DIR" && cargo build --release 2>&1 | tail -3)
  ok "Binary built: $BINARY"
else
  info "Step 2: Skipping build (--skip-build)"
fi

if [ ! -f "$BINARY" ]; then
  fail "Binary not found at $BINARY. Run without --skip-build."
  exit 1
fi

# ── Step 3: Analyze ───────────────────────────────────────────────────────

if [ "$SKIP_KANTRA" = true ]; then
  # Standalone analysis (no kantra)
  info "Step 3: Running standalone analysis"
  "$BINARY" analyze \
    --rules "$RULES_DIR" \
    "$QUIPUCORDS" \
    --output "$ANALYSIS_JSON" \
    --output-format json \
    2>&1 | grep -E "Rules matched|Total incidents|Analysis complete"
  ok "Analysis written to $ANALYSIS_JSON"
else
  # Kantra analysis (with builtin provider for filecontent rules)
  info "Step 3: Running kantra analysis"

  # Write provider settings pointing to the project
  cat > "$WORK_DIR/provider_settings.json" <<EOF
[
  {
    "name": "frontend",
    "address": "localhost:$PROVIDER_PORT",
    "initConfig": [{ "analysisMode": "source-only", "location": "$QUIPUCORDS" }]
  },
  {
    "name": "builtin",
    "initConfig": [{ "location": "$QUIPUCORDS" }]
  }
]
EOF

  # Start gRPC provider
  info "  Starting frontend provider on port $PROVIDER_PORT"
  kill $(lsof -ti :"$PROVIDER_PORT") 2>/dev/null || true
  sleep 1
  "$BINARY" serve --port "$PROVIDER_PORT" 2>"$WORK_DIR/provider.log" &
  PROVIDER_PID=$!
  sleep 2

  if ! kill -0 "$PROVIDER_PID" 2>/dev/null; then
    fail "Provider failed to start. Check $WORK_DIR/provider.log"
    exit 1
  fi
  ok "Provider running (PID $PROVIDER_PID)"

  # Run kantra
  info "  Running kantra analyze"
  rm -rf "$OUTPUT_DIR"

  LABEL_SELECTOR_ARGS=""
  if [ "$INCLUDE_TESTING_RULES" = false ]; then
    LABEL_SELECTOR_ARGS="--label-selector !impact=frontend-testing"
    info "  Excluding frontend-testing rules (use --include-testing-rules to include)"
  fi

  kantra analyze \
    --input "$QUIPUCORDS" \
    --output "$OUTPUT_DIR" \
    --rules "$RULES_DIR" \
    --override-provider-settings "$WORK_DIR/provider_settings.json" \
    --enable-default-rulesets=false \
    --skip-static-report \
    --no-dependency-rules \
    --mode source-only \
    --run-local \
    --provider java \
    $LABEL_SELECTOR_ARGS \
    2>&1 | grep -v "^time=" | grep -v "^\[" || true

  # Stop provider
  info "  Stopping provider"
  kill "$PROVIDER_PID" 2>/dev/null || true
  wait "$PROVIDER_PID" 2>/dev/null || true
  PROVIDER_PID=""

  # Convert YAML → JSON
  if [ -f "$OUTPUT_DIR/output.yaml" ]; then
    yq -o json "$OUTPUT_DIR/output.yaml" > "$ANALYSIS_JSON"
    ok "Analysis written to $ANALYSIS_JSON"
  else
    fail "Kantra did not produce output.yaml"
    exit 1
  fi
fi

# Count incidents
BEFORE_RULES=$(python3 -c "
import json
with open('$ANALYSIS_JSON') as f:
    data = json.load(f)
rules = sum(len(e.get('violations', {})) for e in data)
incidents = sum(len(v.get('incidents', [])) for e in data for v in e.get('violations', {}).values())
print(f'{rules} rules matched, {incidents} incidents')
")
info "  Before fixes: $BEFORE_RULES"

# ── Step 4: Apply pattern-based fixes ──────────────────────────────────────

info "Step 4: Applying pattern-based fixes"
"$BINARY" fix "$QUIPUCORDS" --input "$ANALYSIS_JSON" --apply
ok "Pattern-based fixes applied"

# ── Step 5: Apply goose LLM fixes (optional) ──────────────────────────────

if [ "$WITH_GOOSE" = true ]; then
  info "Step 5: Applying goose LLM fixes"

  # Re-analyze after pattern fixes so goose sees the updated code
  info "  Re-analyzing after pattern fixes"
  "$BINARY" analyze \
    --rules "$RULES_DIR" \
    "$QUIPUCORDS" \
    --output "$WORK_DIR/post-pattern-analysis.json" \
    --output-format json \
    2>&1 | grep -E "Rules matched|Total incidents"

  GOOSE_LOG_ARGS=""
  if [ -n "$LOG_DIR" ]; then
    mkdir -p "$LOG_DIR"
    GOOSE_LOG_ARGS="--log-dir $LOG_DIR"
  fi

  "$BINARY" fix "$QUIPUCORDS" \
    --input "$WORK_DIR/post-pattern-analysis.json" \
    --llm-provider goose \
    --apply \
    --verbose \
    $GOOSE_LOG_ARGS
  ok "Goose LLM fixes applied"
else
  info "Step 5: Skipping goose LLM fixes (use --with-goose to enable)"
fi

# ── Step 6: Re-analyze and measure improvement ────────────────────────────

info "Step 6: Re-analyzing to measure improvement"
"$BINARY" analyze \
  --rules "$RULES_DIR" \
  "$QUIPUCORDS" \
  --output "$WORK_DIR/after-analysis.json" \
  --output-format json \
  2>&1 | grep -E "Rules matched|Total incidents"

python3 <<PYEOF
import json

with open("$ANALYSIS_JSON") as f:
    before = json.load(f)
with open("$WORK_DIR/after-analysis.json") as f:
    after = json.load(f)

before_counts = {}
after_counts = {}
for entry in before:
    for rule_id, v in entry.get("violations", {}).items():
        before_counts[rule_id] = len(v["incidents"])
for entry in after:
    for rule_id, v in entry.get("violations", {}).items():
        after_counts[rule_id] = len(v["incidents"])

total_before = sum(before_counts.values())
total_after = sum(after_counts.values())
resolved = set(before_counts) - set(after_counts)
fixed = total_before - total_after

print()
print("═══════════════════════════════════════════════")
print("  MIGRATION RESULTS")
print("═══════════════════════════════════════════════")
print(f"  Before:  {total_before} incidents across {len(before_counts)} rules")
print(f"  After:   {total_after} incidents across {len(after_counts)} rules")
print(f"  Fixed:   {fixed} incidents ({fixed*100//max(total_before,1)}%)")
print(f"  Rules fully resolved: {len(resolved)}")
print()

if resolved:
    print("  Fully resolved rules:")
    for r in sorted(resolved):
        print(f"    ✓ {r} ({before_counts[r]} incidents)")
    print()

remaining = set(before_counts) & set(after_counts)
if remaining:
    print("  Remaining rules:")
    for r in sorted(remaining):
        b, a = before_counts[r], after_counts[r]
        if a < b:
            print(f"    ◐ {r}: {b} → {a} ({b-a} fixed)")
        else:
            print(f"    ○ {r}: {a} incidents")
print("═══════════════════════════════════════════════")
PYEOF

# ── Step 7: Compare against real v6 ──────────────────────────────────────

info "Step 7: Comparing against real v6 migration"

# We need the original repo (not our modified copy) to access origin/main
ORIGINAL_REPO="/tmp/quipucords-ui"
if [ ! -d "$ORIGINAL_REPO/.git" ]; then
  warn "Original repo not found at $ORIGINAL_REPO, skipping comparison"
  exit 0
fi

python3 <<PYEOF
import subprocess, os

fixtest = "$QUIPUCORDS/src"
v5_repo = "$ORIGINAL_REPO"

our_files = []
for root, dirs, files in os.walk(fixtest):
    for f in files:
        if f.endswith(('.tsx', '.ts', '.css')):
            rel = os.path.relpath(os.path.join(root, f), "$QUIPUCORDS")
            our_files.append(rel)

identical = 0
different = 0

for rel in sorted(our_files):
    our_path = f"$QUIPUCORDS/{rel}"
    try:
        v6_content = subprocess.check_output(
            ["git", "-C", v5_repo, "show", f"origin/main:{rel}"],
            stderr=subprocess.DEVNULL
        ).decode()
    except:
        continue

    with open(our_path) as f:
        our_content = f.read()

    if our_content == v6_content:
        identical += 1
    else:
        different += 1

total = identical + different
print()
print("═══════════════════════════════════════════════")
print("  COMPARISON WITH REAL V6 MIGRATION")
print("═══════════════════════════════════════════════")
print(f"  Files identical to v6:  {identical}/{total} ({identical*100//max(total,1)}%)")
print(f"  Files different:        {different}/{total}")
print("═══════════════════════════════════════════════")
PYEOF

ok "Done! Working directory: $WORK_DIR"
