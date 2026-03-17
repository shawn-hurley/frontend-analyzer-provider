#!/usr/bin/env bash
#
# Show detailed gaps between our automated migration and an official release.
#
# For each file, shows exactly what the official migration did that we missed,
# and what we did differently. Outputs a structured report suitable for
# tracking remaining work.
#
# Usage:
#   ./hack/show-migration-gaps.sh                        # full report
#   ./hack/show-migration-gaps.sh --tag 2.2.0            # specific release tag
#   ./hack/show-migration-gaps.sh --file src/foo.tsx      # single file detail
#   ./hack/show-migration-gaps.sh --only-gaps             # only show files with gaps
#   ./hack/show-migration-gaps.sh --diff                  # show actual diffs
#   ./hack/show-migration-gaps.sh --repo /path/to/repo    # custom repo path
#
# Prerequisites:
#   - The migration-test repo at /tmp/quipucords-migration-test/quipucords-ui
#   - Upstream remote with tags fetched

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
WORK_DIR="/tmp/quipucords-migration-test"
QUIPUCORDS="${WORK_DIR}/quipucords-ui"
UPSTREAM_URL="https://github.com/quipucords/quipucords-ui.git"
RELEASE_TAG="2.2.0"
V5_COMMIT="3b3ce52"
MIGRATION_COMMIT="6a8b6a7"
SINGLE_FILE=""
ONLY_GAPS=false
SHOW_DIFF=false

# ── Parse flags ───────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case $1 in
    --tag) RELEASE_TAG="$2"; shift 2 ;;
    --tag=*) RELEASE_TAG="${1#*=}"; shift ;;
    --migration-commit) MIGRATION_COMMIT="$2"; shift 2 ;;
    --migration-commit=*) MIGRATION_COMMIT="${1#*=}"; shift ;;
    --file) SINGLE_FILE="$2"; shift 2 ;;
    --file=*) SINGLE_FILE="${1#*=}"; shift ;;
    --only-gaps) ONLY_GAPS=true; shift ;;
    --diff) SHOW_DIFF=true; shift ;;
    --repo) QUIPUCORDS="$2"; shift 2 ;;
    --repo=*) QUIPUCORDS="${1#*=}"; shift ;;
    --help|-h)
      sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
      exit 0
      ;;
    *) echo "Unknown flag: $1"; exit 1 ;;
  esac
done

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

header() { echo -e "\n${BOLD}${CYAN}$*${NC}"; }
subheader() { echo -e "\n  ${BOLD}$*${NC}"; }

# ── Validate ──────────────────────────────────────────────────────────────────
if [ ! -d "$QUIPUCORDS/.git" ]; then
  echo -e "${RED}Repository not found at $QUIPUCORDS${NC}"
  exit 1
fi

cd "$QUIPUCORDS"

# Ensure upstream remote
if ! git remote get-url upstream &>/dev/null; then
  git remote add upstream "$UPSTREAM_URL"
fi
git fetch upstream --tags --quiet 2>/dev/null || true

if ! git rev-parse "$RELEASE_TAG" &>/dev/null; then
  echo -e "${RED}Tag '$RELEASE_TAG' not found${NC}"
  exit 1
fi

# ── Gather data ───────────────────────────────────────────────────────────────

# Files changed in the official migration (source only)
OFFICIAL_FILES=$(git diff --name-only "$V5_COMMIT".."$MIGRATION_COMMIT" | grep -E '\.(tsx?|css)$' | sort)

# Files changed in our working tree
OUR_FILES=$(git diff --name-only "$V5_COMMIT" | grep -E '\.(tsx?|css)$' | sort)

# ── Helper: analyze a single file ────────────────────────────────────────────
analyze_file() {
  local file="$1"
  local in_official=false
  local in_ours=false

  echo "$OFFICIAL_FILES" | grep -qF "$file" && in_official=true
  echo "$OUR_FILES" | grep -qF "$file" && in_ours=true

  # Get contents
  local v5_content official_content our_content
  v5_content=$(git show "$V5_COMMIT:$file" 2>/dev/null) || v5_content=""
  official_content=$(git show "$MIGRATION_COMMIT:$file" 2>/dev/null) || official_content=""
  our_content=$(cat "$file" 2>/dev/null) || our_content=""

  # Determine status
  local status=""
  if [ "$in_official" = true ] && [ "$in_ours" = false ]; then
    status="MISSING"
  elif [ "$in_official" = false ] && [ "$in_ours" = true ]; then
    status="EXTRA"
  elif [ "$official_content" = "$our_content" ]; then
    status="MATCH"
  else
    status="GAP"
  fi

  # If --only-gaps, skip MATCH and EXTRA
  if [ "$ONLY_GAPS" = true ] && [ "$status" = "MATCH" ]; then
    return
  fi
  if [ "$ONLY_GAPS" = true ] && [ "$status" = "EXTRA" ]; then
    return
  fi

  # Print file header
  local short_file
  short_file=$(echo "$file" | sed 's|src/||')
  case $status in
    MATCH)   echo -e "\n  ${GREEN}MATCH${NC}    $short_file" ;;
    GAP)     echo -e "\n  ${YELLOW}GAP${NC}      $short_file" ;;
    MISSING) echo -e "\n  ${RED}MISSING${NC}  $short_file" ;;
    EXTRA)   echo -e "\n  ${BLUE}EXTRA${NC}    $short_file" ;;
  esac

  # For MATCH, nothing more to show
  if [ "$status" = "MATCH" ]; then
    echo -e "    ${DIM}Our version is identical to the official migration.${NC}"
    return
  fi

  # For MISSING files: show what the official migration did
  if [ "$status" = "MISSING" ]; then
    local adds dels
    adds=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | grep -c "^+" || echo 0)
    dels=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | grep -c "^-" || echo 0)
    echo "    Official change: +$adds -$dels lines"
    echo "    We did not modify this file."

    if [ "$SHOW_DIFF" = true ]; then
      echo ""
      echo -e "    ${DIM}--- Official migration diff ---${NC}"
      git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | head -80 | sed 's/^/    /'
      local total_lines
      total_lines=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | wc -l | tr -d ' ')
      if [ "$total_lines" -gt 80 ]; then
        echo -e "    ${DIM}... ($((total_lines - 80)) more lines)${NC}"
      fi
    fi
    return
  fi

  # For EXTRA files: show what we changed
  if [ "$status" = "EXTRA" ]; then
    local adds dels
    adds=$(git diff "$V5_COMMIT" -- "$file" | grep -c "^+" || echo 0)
    dels=$(git diff "$V5_COMMIT" -- "$file" | grep -c "^-" || echo 0)
    echo "    Our change: +$adds -$dels lines"
    echo "    This file was not modified in the official migration."

    if [ "$SHOW_DIFF" = true ]; then
      echo ""
      echo -e "    ${DIM}--- Our diff ---${NC}"
      git diff "$V5_COMMIT" -- "$file" | head -60 | sed 's/^/    /'
    fi
    return
  fi

  # For GAP files: show the differences between our version and official
  local official_adds official_dels our_adds our_dels
  official_adds=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | grep -c "^+" || echo 0)
  official_dels=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | grep -c "^-" || echo 0)
  our_adds=$(git diff "$V5_COMMIT" -- "$file" | grep -c "^+" || echo 0)
  our_dels=$(git diff "$V5_COMMIT" -- "$file" | grep -c "^-" || echo 0)

  echo "    Official: +$official_adds -$official_dels    Ours: +$our_adds -$our_dels"

  # Show the diff between our version and the official version
  local gap_diff gap_adds gap_dels
  gap_diff=$(diff <(echo "$official_content") <(echo "$our_content") 2>/dev/null || true)
  gap_adds=$(echo "$gap_diff" | grep -c "^>" || echo 0)
  gap_dels=$(echo "$gap_diff" | grep -c "^<" || echo 0)
  echo "    Remaining gap: $gap_dels lines in official we don't have, $gap_adds lines we have that official doesn't"

  if [ "$SHOW_DIFF" = true ]; then
    echo ""
    echo -e "    ${DIM}--- Diff: official (left) vs ours (right) ---${NC}"
    diff --color=always \
      <(git show "$MIGRATION_COMMIT:$file" 2>/dev/null) \
      <(cat "$file" 2>/dev/null) \
      | head -100 | sed 's/^/    /' || true
    local total_lines
    total_lines=$(diff <(echo "$official_content") <(echo "$our_content") 2>/dev/null | wc -l | tr -d ' ')
    if [ "$total_lines" -gt 100 ]; then
      echo -e "    ${DIM}... ($((total_lines - 100)) more lines)${NC}"
    fi
  fi
}

# ── Main ──────────────────────────────────────────────────────────────────────

header "Migration Gap Analysis: ours vs $RELEASE_TAG (commit $MIGRATION_COMMIT)"
echo -e "${DIM}  Base: $V5_COMMIT | Repo: $QUIPUCORDS${NC}"

if [ -n "$SINGLE_FILE" ]; then
  # Single file mode
  analyze_file "$SINGLE_FILE"
else
  # All files
  ALL_FILES=$(echo -e "${OFFICIAL_FILES}\n${OUR_FILES}" | sort -u | grep -v '^$')

  # Counters
  match=0 gap=0 missing=0 extra=0

  while IFS= read -r file; do
    [ -z "$file" ] && continue

    local_in_official=false
    local_in_ours=false
    echo "$OFFICIAL_FILES" | grep -qF "$file" && local_in_official=true
    echo "$OUR_FILES" | grep -qF "$file" && local_in_ours=true

    if [ "$local_in_official" = true ] && [ "$local_in_ours" = false ]; then
      missing=$((missing + 1))
    elif [ "$local_in_official" = false ] && [ "$local_in_ours" = true ]; then
      extra=$((extra + 1))
    else
      # Both modified -- check content match
      official_content=$(git show "$MIGRATION_COMMIT:$file" 2>/dev/null) || official_content=""
      our_content=$(cat "$file" 2>/dev/null) || our_content=""
      if [ "$official_content" = "$our_content" ]; then
        match=$((match + 1))
      else
        gap=$((gap + 1))
      fi
    fi

    analyze_file "$file"
  done <<< "$ALL_FILES"

  # ── Summary table ────────────────────────────────────────────────────────
  header "Summary"
  total=$((match + gap + missing + extra))
  official_total=$((match + gap + missing))

  echo ""
  echo -e "  ${GREEN}Exact match:${NC}     $match"
  echo -e "  ${YELLOW}Has gaps:${NC}        $gap"
  echo -e "  ${RED}Missing:${NC}         $missing"
  echo -e "  ${BLUE}Extra:${NC}           $extra"
  echo ""

  if [ "$official_total" -gt 0 ]; then
    coverage=$((match * 100 / official_total))
    echo "  Exact match rate:  ${coverage}% ($match / $official_total official files)"
    touched=$(( (match + gap) * 100 / official_total))
    echo "  Coverage rate:     ${touched}% ($(( match + gap )) / $official_total official files)"
  fi

  # ── Actionable summary ──────────────────────────────────────────────────
  if [ "$gap" -gt 0 ] || [ "$missing" -gt 0 ]; then
    header "Action Items"

    if [ "$missing" -gt 0 ]; then
      echo ""
      echo -e "  ${RED}Files to add to migration rules ($missing):${NC}"
      while IFS= read -r file; do
        [ -z "$file" ] && continue
        echo "$OUR_FILES" | grep -qF "$file" && continue
        echo "$OFFICIAL_FILES" | grep -qF "$file" || continue
        short_file=$(echo "$file" | sed 's|src/||')
        echo "    - $short_file"
      done <<< "$ALL_FILES"
    fi

    if [ "$gap" -gt 0 ]; then
      echo ""
      echo -e "  ${YELLOW}Files needing improvement ($gap):${NC}"
      while IFS= read -r file; do
        [ -z "$file" ] && continue
        local_in_official=false
        local_in_ours=false
        echo "$OFFICIAL_FILES" | grep -qF "$file" && local_in_official=true
        echo "$OUR_FILES" | grep -qF "$file" && local_in_ours=true
        [ "$local_in_official" = false ] && continue
        [ "$local_in_ours" = false ] && continue

        official_content=$(git show "$MIGRATION_COMMIT:$file" 2>/dev/null) || official_content=""
        our_content=$(cat "$file" 2>/dev/null) || our_content=""
        [ "$official_content" = "$our_content" ] && continue

        short_file=$(echo "$file" | sed 's|src/||')
        gap_lines=$(diff <(echo "$official_content") <(echo "$our_content") 2>/dev/null | grep -cE "^[<>]" || true)
        gap_lines=$(echo "$gap_lines" | tr -d '[:space:]')
        gap_lines=${gap_lines:-0}
        echo "    - $short_file  ($gap_lines differing lines)"
      done <<< "$ALL_FILES"
    fi
  fi

  echo ""
fi

echo -e "${DIM}Tip: Use --diff to see actual diffs, --only-gaps to hide matching files, --file <path> for single file detail${NC}"
