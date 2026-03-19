#!/usr/bin/env bash
#
# Full PF v5→v6 migration pipeline against quipucords-ui.
#
# Steps:
#   1. Clone quipucords-ui (or use existing) and reset to v5 commit
#   2. Build the provider
#   3. Start provider + run kantra analysis
#   4. Apply pattern-based fixes
#   5. Apply LLM fixes (enabled by default, skip with --no-llm)
#   6. Re-analyze with kantra to measure improvement
#   7. Compare against real v6 migration
#
# Usage:
#   ./hack/run-full-migration.sh                        # pattern + LLM fixes (default)
#   ./hack/run-full-migration.sh --no-llm               # pattern fixes only, skip LLM
#   ./hack/run-full-migration.sh --skip-build           # skip cargo build
#   ./hack/run-full-migration.sh --include-testing-rules # include DOM/CSS/a11y/behavioral proxy rules
#   ./hack/run-full-migration.sh --llm-provider openai  # use OpenAI-compatible endpoint instead of goose

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORK_DIR="/tmp/quipucords-migration-test"
REPO_URL="git@github.com:jwmatthews/quipucords-ui.git"
V5_COMMIT="3b3ce52"
PROVIDER_PORT=9001
PROVIDER_PID=""

# Parse flags
WITH_LLM=true
LLM_PROVIDER="goose"
SKIP_BUILD=false
INCLUDE_TESTING_RULES=false
LOG_DIR=""
for arg in "$@"; do
  case $arg in
    --with-goose) WITH_LLM=true; LLM_PROVIDER="goose" ;;  # backward compat
    --with-llm) WITH_LLM=true ;;                            # explicit enable
    --no-llm|--skip-llm) WITH_LLM=false ;;
    --llm-provider=*) LLM_PROVIDER="${arg#*=}"; WITH_LLM=true ;;
    --skip-build) SKIP_BUILD=true ;;
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
RULES_STRATEGIES="$RULES_DIR/fix-strategies.json"
PROVIDER_SETTINGS="$PROJECT_DIR/provider_settings.json"
QUIPUCORDS="$WORK_DIR/quipucords-ui"
OUTPUT_DIR="$WORK_DIR/analysis-output"
ANALYSIS_JSON="$WORK_DIR/analysis.json"

# ── Step 1: Clone / Reset quipucords-ui ────────────────────────────────────

info "Step 1: Preparing quipucords-ui at v5 commit ($V5_COMMIT)"

if [ -d "$QUIPUCORDS/.git" ]; then
  info "  Repository exists, hard-resetting to $V5_COMMIT"
  git -C "$QUIPUCORDS" checkout "$V5_COMMIT" --force 2>/dev/null
  git -C "$QUIPUCORDS" clean -fd 2>/dev/null
else
  info "  Cloning from $REPO_URL"
  git clone --quiet "$REPO_URL" "$QUIPUCORDS"
  git -C "$QUIPUCORDS" checkout "$V5_COMMIT" --force 2>/dev/null
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

# ── Analysis helper ────────────────────────────────────────────────────────
# Runs kantra analysis and writes JSON output.
# Usage: run_kantra_analysis <output_json> <label>
run_kantra_analysis() {
  local out_json="$1"
  local label="$2"
  local kantra_out="$WORK_DIR/kantra-output-${label}"

  # Ensure provider is running
  if [ -z "$PROVIDER_PID" ] || ! kill -0 "$PROVIDER_PID" 2>/dev/null; then
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
    ok "  Provider running (PID $PROVIDER_PID)"
  fi

  rm -rf "$kantra_out"

  LABEL_SELECTOR_ARGS=""
  if [ "$INCLUDE_TESTING_RULES" = false ]; then
    LABEL_SELECTOR_ARGS="--label-selector !impact=frontend-testing"
  fi

  kantra analyze \
    --input "$QUIPUCORDS" \
    --output "$kantra_out" \
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

  if [ -f "$kantra_out/output.yaml" ]; then
    yq -o json "$kantra_out/output.yaml" > "$out_json"
  else
    fail "Kantra did not produce output.yaml for $label"
    exit 1
  fi
}

# ── Step 3: Start provider + initial analysis ──────────────────────────────

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

if [ "$INCLUDE_TESTING_RULES" = false ]; then
  info "  Excluding frontend-testing rules (use --include-testing-rules to include)"
fi

run_kantra_analysis "$ANALYSIS_JSON" "initial"
ok "Analysis written to $ANALYSIS_JSON"

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
"$BINARY" fix "$QUIPUCORDS" --input "$ANALYSIS_JSON" --apply \
  --rules-strategies "$RULES_STRATEGIES"
ok "Pattern-based fixes applied"

# ── Step 5: Apply LLM fixes ────────────────────────────────────────────────

if [ "$WITH_LLM" = true ]; then
  info "Step 5: Applying LLM fixes (provider: $LLM_PROVIDER)"

  # Re-analyze after pattern fixes so the LLM sees the updated code
  info "  Re-analyzing after pattern fixes (kantra)"
  run_kantra_analysis "$WORK_DIR/post-pattern-analysis.json" "post-pattern"

  POST_PATTERN_INCIDENTS=$(python3 -c "
import json
with open('$WORK_DIR/post-pattern-analysis.json') as f:
    data = json.load(f)
incidents = sum(len(v.get('incidents', [])) for e in data for v in e.get('violations', {}).values())
print(incidents)
")
  info "  Remaining incidents after pattern fixes: $POST_PATTERN_INCIDENTS"

  LLM_EXTRA_ARGS=""
  if [ -n "$LOG_DIR" ]; then
    mkdir -p "$LOG_DIR"
    LLM_EXTRA_ARGS="--log-dir $LOG_DIR"
  fi

  LLM_EXIT=0
  "$BINARY" fix "$QUIPUCORDS" \
    --input "$WORK_DIR/post-pattern-analysis.json" \
    --rules-strategies "$RULES_STRATEGIES" \
    --llm-provider "$LLM_PROVIDER" \
    --apply \
    --verbose \
    $LLM_EXTRA_ARGS || LLM_EXIT=$?

  if [ "$LLM_EXIT" -eq 0 ]; then
    ok "LLM fixes applied (provider: $LLM_PROVIDER)"
  else
    warn "LLM fix step exited with code $LLM_EXIT (interrupted or error)"
    warn "Continuing with partial LLM fixes applied so far"
  fi
else
  info "Step 5: Skipping LLM fixes (use --with-llm to enable, or remove --no-llm)"
fi

# ── Step 6: Re-analyze and measure improvement ────────────────────────────

info "Step 6: Re-analyzing to measure improvement (kantra)"
run_kantra_analysis "$WORK_DIR/after-analysis.json" "final"

python3 <<PYEOF
import json, os

with open("$ANALYSIS_JSON") as f:
    before = json.load(f)
with open("$WORK_DIR/after-analysis.json") as f:
    after = json.load(f)

# Load post-pattern analysis if LLM fixes were applied
post_pattern_path = "$WORK_DIR/post-pattern-analysis.json"
post_pattern_counts = None
if os.path.exists(post_pattern_path) and "$WITH_LLM" == "true":
    with open(post_pattern_path) as f:
        post_pattern = json.load(f)
    post_pattern_counts = {}
    for entry in post_pattern:
        for rule_id, v in entry.get("violations", {}).items():
            post_pattern_counts[rule_id] = len(v["incidents"])

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
pct = fixed * 100 // max(total_before, 1)

print()
print("═══════════════════════════════════════════════════════════")
print("  MIGRATION RESULTS")
print("═══════════════════════════════════════════════════════════")
print(f"  Initial analysis:   {total_before} incidents across {len(before_counts)} rules")

if post_pattern_counts is not None:
    total_post_pattern = sum(post_pattern_counts.values())
    pattern_fixed = total_before - total_post_pattern
    pattern_pct = pattern_fixed * 100 // max(total_before, 1)
    llm_fixed = total_post_pattern - total_after
    llm_pct = llm_fixed * 100 // max(total_post_pattern, 1)
    print(f"  After pattern fixes: {total_post_pattern} incidents ({pattern_fixed} fixed, {pattern_pct}%)")
    print(f"  After LLM fixes:    {total_after} incidents ({llm_fixed} fixed, {llm_pct}% of remaining)")
else:
    print(f"  After fixes:        {total_after} incidents across {len(after_counts)} rules")

print(f"  ─────────────────────────────────────────────")
print(f"  Total fixed:        {fixed} / {total_before} incidents ({pct}%)")
print(f"  Remaining:          {total_after} incidents across {len(after_counts)} rules")
print(f"  Rules fully resolved: {len(resolved)} / {len(before_counts)}")
print()

if resolved:
    print("  Fully resolved rules:")
    for r in sorted(resolved):
        print(f"    ✓ {r} ({before_counts[r]} incidents)")
    print()

partially_fixed = []
unfixed = []
remaining = set(before_counts) & set(after_counts)
for r in sorted(remaining):
    b, a = before_counts[r], after_counts[r]
    if a < b:
        partially_fixed.append((r, b, a))
    else:
        unfixed.append((r, a))

if partially_fixed:
    print("  Partially fixed rules:")
    for r, b, a in partially_fixed:
        print(f"    ◐ {r}: {b} → {a} ({b-a} fixed, {a} remaining)")
    print()

if unfixed:
    print("  Unfixed rules (no change):")
    for r, a in unfixed:
        print(f"    ○ {r}: {a} incidents")
    print()

print("═══════════════════════════════════════════════════════════")
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
