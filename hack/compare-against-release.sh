#!/usr/bin/env bash
#
# Compare our automated PF v5->v6 migration against an official quipucords-ui release.
#
# This script compares the current working tree of the migration-test repo
# against the official upstream release tag to assess migration quality.
#
# It compares only the PF6 migration commit (6a8b6a7) from the upstream release,
# filtering out post-migration feature work.
#
# Usage:
#   ./hack/compare-against-release.sh                    # compare against 2.2.0 (default)
#   ./hack/compare-against-release.sh --tag 2.3.0        # compare against a different tag
#   ./hack/compare-against-release.sh --migration-commit abc1234  # override migration commit
#   ./hack/compare-against-release.sh --repo /path/to/repo        # custom repo path
#   ./hack/compare-against-release.sh --gaps                      # include per-file gap details for partial/different files
#   ./hack/compare-against-release.sh --diffs                     # write per-file diffs to /tmp/quipucords-migration-test/diffs/
#   ./hack/compare-against-release.sh --diff-dir /path/to/dir     # write per-file diffs to custom directory
#
# Prerequisites:
#   - The migration-test repo must exist at /tmp/quipucords-migration-test/quipucords-ui
#   - The upstream remote (https://github.com/quipucords/quipucords-ui.git) must be added
#     and fetched with tags

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
WORK_DIR="/tmp/quipucords-migration-test"
QUIPUCORDS="${WORK_DIR}/quipucords-ui"
UPSTREAM_URL="https://github.com/quipucords/quipucords-ui.git"
RELEASE_TAG="2.2.0"
V5_COMMIT="3b3ce52"
# The commit in the upstream repo that contains the core PF6 migration
MIGRATION_COMMIT="6a8b6a7"
SHOW_GAPS=false
DIFF_DIR=""

# ── Parse flags ───────────────────────────────────────────────────────────────
for arg in "$@"; do
  case $arg in
    --tag) shift; RELEASE_TAG="$1"; shift ;;
    --tag=*) RELEASE_TAG="${arg#*=}" ;;
    --migration-commit) shift; MIGRATION_COMMIT="$1"; shift ;;
    --migration-commit=*) MIGRATION_COMMIT="${arg#*=}" ;;
    --repo) shift; QUIPUCORDS="$1"; shift ;;
    --repo=*) QUIPUCORDS="${arg#*=}" ;;
    --gaps) SHOW_GAPS=true ;;
    --diff-dir) shift; DIFF_DIR="$1"; shift ;;
    --diff-dir=*) DIFF_DIR="${arg#*=}" ;;
    --diffs) DIFF_DIR="${WORK_DIR}/diffs" ;;
    --help|-h)
      sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
      exit 0
      ;;
  esac
done

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
fail()  { echo -e "${RED}[FAIL]${NC}  $*"; }
header() { echo -e "\n${BOLD}${CYAN}$*${NC}"; }

# ── Validate ──────────────────────────────────────────────────────────────────
if [ ! -d "$QUIPUCORDS/.git" ]; then
  fail "Repository not found at $QUIPUCORDS"
  exit 1
fi

cd "$QUIPUCORDS"

# Ensure upstream remote exists
if ! git remote get-url upstream &>/dev/null; then
  info "Adding upstream remote: $UPSTREAM_URL"
  git remote add upstream "$UPSTREAM_URL"
fi

# Fetch tags
info "Fetching upstream tags..."
git fetch upstream --tags --quiet 2>/dev/null || {
  warn "Could not fetch upstream. Using existing local data."
}

# Verify the tag exists
if ! git rev-parse "$RELEASE_TAG" &>/dev/null; then
  fail "Tag '$RELEASE_TAG' not found. Available tags:"
  git tag -l '2.*' | sort -V | sed 's/^/  /'
  exit 1
fi

# ── Gather file lists ─────────────────────────────────────────────────────────
header "Comparing migration against upstream $RELEASE_TAG"
info "Base commit (v5): $V5_COMMIT"
info "Migration commit: $MIGRATION_COMMIT"
info "Release tag:      $RELEASE_TAG"

# Files changed in the official migration commit (source files only)
OFFICIAL_FILES=$(git diff --name-only "$V5_COMMIT".."$MIGRATION_COMMIT" 2>/dev/null | grep -E '\.(tsx?|css)$' | sort)
OFFICIAL_COUNT=$(echo "$OFFICIAL_FILES" | grep -c . 2>/dev/null || true)
OFFICIAL_COUNT=${OFFICIAL_COUNT:-0}

# Files changed in our working tree vs the v5 base
OUR_FILES=$(git diff --name-only "$V5_COMMIT" 2>/dev/null | grep -E '\.(tsx?|css)$' | sort)
OUR_COUNT=$(echo "$OUR_FILES" | grep -c . 2>/dev/null || true)
OUR_COUNT=${OUR_COUNT:-0}

# Overlap
OVERLAP=$(comm -12 <(echo "$OFFICIAL_FILES") <(echo "$OUR_FILES"))
OVERLAP_COUNT=$(echo "$OVERLAP" | grep -c . 2>/dev/null || true)
OVERLAP_COUNT=${OVERLAP_COUNT:-0}

# Files only in official
ONLY_OFFICIAL=$(comm -23 <(echo "$OFFICIAL_FILES") <(echo "$OUR_FILES"))
ONLY_OFFICIAL_COUNT=$(echo "$ONLY_OFFICIAL" | grep -c . 2>/dev/null || true)
ONLY_OFFICIAL_COUNT=${ONLY_OFFICIAL_COUNT:-0}

# Files only in ours
ONLY_OURS=$(comm -13 <(echo "$OFFICIAL_FILES") <(echo "$OUR_FILES"))
ONLY_OURS_COUNT=$(echo "$ONLY_OURS" | grep -c . 2>/dev/null || true)
ONLY_OURS_COUNT=${ONLY_OURS_COUNT:-0}

header "File Coverage"
echo "  Official migration files:  $OFFICIAL_COUNT"
echo "  Our migration files:       $OUR_COUNT"
echo "  Overlap:                   $OVERLAP_COUNT"
echo "  We missed:                 $ONLY_OFFICIAL_COUNT"
echo "  Extra (not in official):   $ONLY_OURS_COUNT"

# ── Per-file comparison ──────────────────────────────────────────────────────
header "Per-File Comparison (overlapping files)"

match_count=0
partial_count=0
different_count=0

while IFS= read -r file; do
  [ -z "$file" ] && continue

  # Get official diff line count
  official_diff=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" 2>/dev/null)
  official_adds=$(echo "$official_diff" | grep -c "^+" || echo 0)
  official_dels=$(echo "$official_diff" | grep -c "^-" || echo 0)

  # Get our diff line count
  our_diff=$(git diff "$V5_COMMIT" -- "$file" 2>/dev/null)
  our_adds=$(echo "$our_diff" | grep -c "^+" || echo 0)
  our_dels=$(echo "$our_diff" | grep -c "^-" || echo 0)

  # Compare our version against the official version at the migration commit
  official_content=$(git show "$MIGRATION_COMMIT:$file" 2>/dev/null) || official_content=""
  our_content=$(cat "$file" 2>/dev/null) || our_content=""

  if [ "$official_content" = "$our_content" ]; then
    rating="MATCH"
    match_count=$((match_count + 1))
    marker="${GREEN}MATCH${NC}"
  else
    # Check how similar they are by comparing diff sizes
    # If our diff covers most of the official changes, it's partial
    # Get a similarity score: lines that match between our version and official
    diff_lines=$(diff <(echo "$official_content") <(echo "$our_content") 2>/dev/null | grep -cE "^[<>]" || true)
    diff_lines=${diff_lines:-0}
    diff_lines=$(echo "$diff_lines" | tr -d '[:space:]')
    total_lines=$(echo "$official_content" | wc -l)
    total_lines=$(echo "$total_lines" | tr -d '[:space:]')
    total_lines=${total_lines:-0}

    if [ "$total_lines" -gt 0 ] 2>/dev/null; then
      denom=$((total_lines * 2 + 1))
      similarity=$((100 - (diff_lines * 100 / denom)))
      # Clamp to 0-100
      [ "$similarity" -lt 0 ] && similarity=0
      [ "$similarity" -gt 100 ] && similarity=100
    else
      similarity=0
    fi

    if [ "$similarity" -ge 90 ]; then
      rating="PARTIAL"
      partial_count=$((partial_count + 1))
      marker="${YELLOW}PARTIAL${NC} (${similarity}% similar)"
    elif [ "$similarity" -ge 70 ]; then
      rating="PARTIAL"
      partial_count=$((partial_count + 1))
      marker="${YELLOW}PARTIAL${NC} (${similarity}% similar)"
    else
      rating="DIFFERENT"
      different_count=$((different_count + 1))
      marker="${RED}DIFFERENT${NC} (${similarity}% similar)"
    fi
  fi

  # Shorten file path for display
  short_file=$(echo "$file" | sed 's|src/||; s|vendor/react-table-batteries/||; s|tackle2-ui-legacy/||')
  echo -e "  ${short_file}"
  echo -e "    [$marker]  official: +${official_adds} -${official_dels}  |  ours: +${our_adds} -${our_dels}"

done <<< "$OVERLAP"

# ── Summary ──────────────────────────────────────────────────────────────────
header "Summary"
total=$((match_count + partial_count + different_count))

echo ""
echo -e "  ${GREEN}Match:${NC}     $match_count / $total  ($(( match_count * 100 / (total > 0 ? total : 1) ))%)"
echo -e "  ${YELLOW}Partial:${NC}   $partial_count / $total  ($(( partial_count * 100 / (total > 0 ? total : 1) ))%)"
echo -e "  ${RED}Different:${NC} $different_count / $total  ($(( different_count * 100 / (total > 0 ? total : 1) ))%)"
echo ""

# File coverage score
if [ "$OFFICIAL_COUNT" -gt 0 ]; then
  coverage=$((OVERLAP_COUNT * 100 / OFFICIAL_COUNT))
else
  coverage=0
fi
echo "  File coverage: ${OVERLAP_COUNT}/${OFFICIAL_COUNT} (${coverage}%)"

echo ""

# ── Files we missed ──────────────────────────────────────────────────────────
if [ "$ONLY_OFFICIAL_COUNT" -gt 0 ]; then
  header "Files in official migration that we missed ($ONLY_OFFICIAL_COUNT)"
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    adds=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | grep -c "^+" || echo 0)
    dels=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" | grep -c "^-" || echo 0)
    short_file=$(echo "$file" | sed 's|src/||')
    printf "  %-55s +%-4d -%-4d\n" "$short_file" "$adds" "$dels"
  done <<< "$ONLY_OFFICIAL"
  echo ""
fi

# ── Extra files we changed ───────────────────────────────────────────────────
if [ "$ONLY_OURS_COUNT" -gt 0 ]; then
  header "Extra files we changed (not in official migration) ($ONLY_OURS_COUNT)"
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    adds=$(git diff "$V5_COMMIT" -- "$file" | grep -c "^+" || echo 0)
    dels=$(git diff "$V5_COMMIT" -- "$file" | grep -c "^-" || echo 0)
    short_file=$(echo "$file" | sed 's|src/||')
    printf "  %-55s +%-4d -%-4d\n" "$short_file" "$adds" "$dels"
  done <<< "$ONLY_OURS"
  echo ""
fi

# ── Missing changes chart ────────────────────────────────────────────────────
header "Changes in official migration missing from ours"
echo ""
printf "  ${BOLD}%-50s  %-8s  %s${NC}\n" "FILE" "STATUS" "MISSING CHANGES"
printf "  %-50s  %-8s  %s\n" "$(printf '%.0s─' {1..50})" "────────" "$(printf '%.0s─' {1..60})"

# Always show: files we missed entirely
while IFS= read -r file; do
  [ -z "$file" ] && continue
  short_file=$(echo "$file" | sed 's|src/||; s|vendor/react-table-batteries/||; s|tackle2-ui-legacy/||')
  # Summarize what the official diff does
  official_diff=$(git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" 2>/dev/null)
  # Extract meaningful changes (added lines, minus diff headers)
  changes=$(echo "$official_diff" | grep "^+" | grep -v "^+++" | head -20 | sed 's/^+//' | tr -s '[:space:]' ' ')
  # Summarize by looking for key patterns
  summary=""
  echo "$official_diff" | grep -q "import" && summary="${summary}imports, "
  echo "$official_diff" | grep -q "className\|pf-v[56]" && summary="${summary}CSS classes, "
  echo "$official_diff" | grep -q "variant\|size\|icon" && summary="${summary}prop changes, "
  echo "$official_diff" | grep -q "filter:\|theme" && summary="${summary}theme/styling, "
  echo "$official_diff" | grep -q "Modal\|ModalHeader\|ModalBody" && summary="${summary}Modal migration, "
  echo "$official_diff" | grep -q "EmptyState\|titleText" && summary="${summary}EmptyState, "
  echo "$official_diff" | grep -q "screen\.\|getByRole\|waitFor" && summary="${summary}test selectors, "
  echo "$official_diff" | grep -q "alignEnd\|alignRight" && summary="${summary}alignment renames, "
  summary=${summary%, }
  [ -z "$summary" ] && summary="(see diff)"

  printf "  %-50s  ${RED}%-8s${NC}  %s\n" "$short_file" "MISSING" "$summary"
done <<< "$ONLY_OFFICIAL"

# Optionally: files with gaps (partial/different)
if [ "$SHOW_GAPS" = true ]; then
while IFS= read -r file; do
  [ -z "$file" ] && continue

  official_content=$(git show "$MIGRATION_COMMIT:$file" 2>/dev/null) || continue
  our_content=$(cat "$file" 2>/dev/null) || continue
  [ "$official_content" = "$our_content" ] && continue

  short_file=$(echo "$file" | sed 's|src/||; s|vendor/react-table-batteries/||; s|tackle2-ui-legacy/||')

  # Get lines that are in official but not in ours
  missing_lines=$(diff <(echo "$our_content") <(echo "$official_content") 2>/dev/null | grep "^>" | sed 's/^> //' || true)
  missing_count=$(echo "$missing_lines" | grep -c . 2>/dev/null || true)
  missing_count=$(echo "$missing_count" | tr -d '[:space:]')

  # Get lines we have that official doesn't
  extra_lines=$(diff <(echo "$our_content") <(echo "$official_content") 2>/dev/null | grep "^<" | sed 's/^< //' || true)

  # Categorize the missing changes
  summary=""
  { echo "$missing_lines" | grep -q "import.*deprecated\|from.*deprecated"; } 2>/dev/null && summary="${summary}deprecated import path, " || true
  { echo "$missing_lines" | grep -q "ModalHeader\|ModalBody\|ModalFooter"; } 2>/dev/null && summary="${summary}Modal API (deprecated shim), " || true
  { echo "$missing_lines" | grep -q "hasBodyWrapper"; } 2>/dev/null && summary="${summary}hasBodyWrapper={false}, " || true
  { echo "$missing_lines" | grep -q 'size="sm"'; } 2>/dev/null && summary="${summary}size=\"sm\", " || true
  { echo "$missing_lines" | grep -q "hasAction"; } 2>/dev/null && summary="${summary}hasAction prop, " || true
  { echo "$missing_lines" | grep -q "titleText\|Title "; } 2>/dev/null && summary="${summary}Title wrapping, " || true
  { echo "$missing_lines" | grep -q "Icon.*status\|<Icon "; } 2>/dev/null && summary="${summary}Icon status wrapper, " || true
  { echo "$missing_lines" | grep -q "async\|await\|waitFor"; } 2>/dev/null && summary="${summary}async test fixes, " || true
  { echo "$missing_lines" | grep -q "popperProps\|ouiaId"; } 2>/dev/null && summary="${summary}popperProps/ouiaId, " || true
  { echo "$missing_lines" | grep -q "isExpandable\|hasAnimations"; } 2>/dev/null && summary="${summary}table props, " || true
  { echo "$missing_lines" | grep -q "Avatar\|avatar"; } 2>/dev/null && summary="${summary}Avatar component, " || true
  { echo "$missing_lines" | grep -q "pf-v6-theme-dark\|pf-v6-c-avatar"; } 2>/dev/null && summary="${summary}theme classes, " || true
  { echo "$missing_lines" | grep -q "alignEnd"; } 2>/dev/null && summary="${summary}align renames, " || true
  { echo "$missing_lines" | grep -q "PageSection\|variant.*light"; } 2>/dev/null && summary="${summary}PageSection variant, " || true
  { echo "$missing_lines" | grep -q "EmptyState\|EmptyStateHeader\|EmptyStateIcon"; } 2>/dev/null && summary="${summary}EmptyState, " || true

  # Note where our approach differs (not missing, just different)
  { echo "$extra_lines" | grep -q "ModalHeader\|ModalBody\|ModalFooter"; } 2>/dev/null && summary="${summary}(we use new Modal API), " || true
  { echo "$extra_lines" | grep -q "Badge\|hasCheckbox\|textFilter"; } 2>/dev/null && summary="${summary}(we add extra features), " || true
  { echo "$extra_lines" | grep -q 'status="danger"'; } 2>/dev/null && summary="${summary}(we add status prop), " || true

  summary=${summary%, }
  [ -z "$summary" ] && summary="${missing_count:-0} differing lines"

  printf "  %-50s  ${YELLOW}%-8s${NC}  %s\n" "$short_file" "GAP" "$summary"
done <<< "$OVERLAP"
fi # SHOW_GAPS

echo ""

# ── Write per-file diffs to directory ────────────────────────────────────────
if [ -n "$DIFF_DIR" ]; then
  rm -rf "$DIFF_DIR"
  mkdir -p "$DIFF_DIR/missing" "$DIFF_DIR/gaps" "$DIFF_DIR/extra"

  header "Writing diffs to $DIFF_DIR"

  # Missing files: show what official changed (we have nothing)
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    safe_name=$(echo "$file" | sed 's|/|__|g')
    {
      echo "# MISSING: $file"
      echo "# This file was changed in the official migration but we did not modify it."
      echo "# Diff shows official changes from v5 base ($V5_COMMIT) to migration ($MIGRATION_COMMIT)."
      echo ""
      git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" 2>/dev/null
    } > "$DIFF_DIR/missing/${safe_name}.diff"
  done <<< "$ONLY_OFFICIAL"

  # Overlapping files: show diff between our version and official version
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    official_content=$(git show "$MIGRATION_COMMIT:$file" 2>/dev/null) || continue
    our_content=$(cat "$file" 2>/dev/null) || continue

    safe_name=$(echo "$file" | sed 's|/|__|g')

    if [ "$official_content" = "$our_content" ]; then
      # Exact match — write a note
      {
        echo "# MATCH: $file"
        echo "# Our version is identical to the official migration."
      } > "$DIFF_DIR/gaps/${safe_name}.diff"
    else
      # Gap — show three diffs for full context
      {
        echo "# GAP: $file"
        echo "# Our version differs from the official migration."
        echo ""
        echo "###############################################################################"
        echo "# 1. Official migration changes (v5 -> official)"
        echo "###############################################################################"
        echo ""
        git diff "$V5_COMMIT".."$MIGRATION_COMMIT" -- "$file" 2>/dev/null
        echo ""
        echo "###############################################################################"
        echo "# 2. Our migration changes (v5 -> ours)"
        echo "###############################################################################"
        echo ""
        git diff "$V5_COMMIT" -- "$file" 2>/dev/null
        echo ""
        echo "###############################################################################"
        echo "# 3. Diff between our version and official version"
        echo "#    Lines starting with < are in our version only"
        echo "#    Lines starting with > are in official version only"
        echo "###############################################################################"
        echo ""
        diff -u <(echo "$our_content") <(echo "$official_content") \
          --label "ours: $file" --label "official: $file" 2>/dev/null || true
      } > "$DIFF_DIR/gaps/${safe_name}.diff"
    fi
  done <<< "$OVERLAP"

  # Extra files: show what we changed (official didn't)
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    safe_name=$(echo "$file" | sed 's|/|__|g')
    {
      echo "# EXTRA: $file"
      echo "# We modified this file but the official migration did not."
      echo "# Diff shows our changes from v5 base ($V5_COMMIT)."
      echo ""
      git diff "$V5_COMMIT" -- "$file" 2>/dev/null
    } > "$DIFF_DIR/extra/${safe_name}.diff"
  done <<< "$ONLY_OURS"

  # Summary
  missing_n=$(ls "$DIFF_DIR/missing/"*.diff 2>/dev/null | wc -l | tr -d ' ')
  gaps_n=$(ls "$DIFF_DIR/gaps/"*.diff 2>/dev/null | wc -l | tr -d ' ')
  extra_n=$(ls "$DIFF_DIR/extra/"*.diff 2>/dev/null | wc -l | tr -d ' ')
  echo "  $DIFF_DIR/missing/  — $missing_n files (official changed, we didn't)"
  echo "  $DIFF_DIR/gaps/     — $gaps_n files (both changed, showing differences)"
  echo "  $DIFF_DIR/extra/    — $extra_n files (we changed, official didn't)"
  echo ""
fi

ok "Comparison complete"
