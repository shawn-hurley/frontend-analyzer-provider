# frontend-analyzer-provider

A [Konveyor](https://www.konveyor.io/) external provider for semantic analysis of JavaScript/TypeScript/JSX/TSX and CSS/SCSS frontend codebases. Its primary use case is automating **PatternFly v5 to v6 migration** -- detecting breaking changes and applying fixes, both deterministically and via LLM-assisted code generation.

## How It Works

The provider runs as a **gRPC server** that plugs into [kantra](https://github.com/konveyor/kantra) (the Konveyor analysis CLI). Kantra sends analysis requests using rules you author in Konveyor YAML format, and the provider scans your frontend source code for matching patterns -- component usage, prop values, CSS classes, CSS variables, and dependency versions.

After analysis produces a report of violations, the **fix engine** can automatically apply corrections: simple renames and prop removals are handled deterministically, while complex structural changes can be delegated to an LLM (Goose or an OpenAI-compatible endpoint).

## Prerequisites

- **Rust toolchain** (edition 2021)
- **kantra** CLI ([install instructions](https://github.com/konveyor/kantra))
- **goose** CLI (only if using LLM-assisted fixes with the `goose` provider)

## Building

```bash
# Debug build
cargo build

# Release build
cargo build --release
```

The binary is produced at `target/release/frontend-analyzer-provider`.

---

## Running as a Kantra Provider

This is the primary way to use the tool. You start the gRPC provider server, then point kantra at it to analyze your project.

### 1. Start the provider

```bash
frontend-analyzer-provider serve --port 9001
```

The server listens for gRPC connections from kantra. The default port (if `--port` is omitted) is `9090`. You can also use a Unix socket instead:

```bash
frontend-analyzer-provider serve --socket /tmp/frontend-provider.sock
```

### 2. Create provider settings

Kantra needs a JSON file that tells it how to reach your provider. Create a `provider_settings.json`:

```json
[
    {
        "name": "frontend",
        "address": "localhost:9001",
        "initConfig": [
            {
                "analysisMode": "source-only",
                "location": "/path/to/your/project"
            }
        ]
    },
    {
        "name": "builtin",
        "initConfig": [
            {
                "location": "/path/to/your/project"
            }
        ]
    }
]
```

The `builtin` provider entry is needed for any rules that use `filecontent` conditions (regex-based text matching). Both entries should point `location` at the root of the project you want to analyze.

### 3. Run kantra

```bash
kantra analyze \
  --input /path/to/your/project \
  --output /path/to/output \
  --rules rules/patternfly-v5-to-v6 \
  --override-provider-settings provider_settings.json \
  --enable-default-rulesets=false \
  --skip-static-report \
  --no-dependency-rules \
  --mode source-only \
  --run-local \
  --provider java \
  --label-selector '!impact=frontend-testing'
```

| Flag | Why |
|---|---|
| `--rules` | Path to the rules directory (e.g., `rules/patternfly-v5-to-v6`) |
| `--override-provider-settings` | Points kantra at your running provider |
| `--enable-default-rulesets=false` | Disables kantra's built-in Java/Go rulesets |
| `--skip-static-report` | Skips HTML report generation |
| `--no-dependency-rules` | Skips dependency-only rules |
| `--mode source-only` | Analyzes source code, not compiled artifacts |
| `--run-local` | Runs kantra locally (no container) |
| `--provider java` | Required flag for kantra, but analysis uses your external provider |
| `--label-selector` | `!impact=frontend-testing` excludes proxy/testing rules |

Kantra writes its output to `output/output.yaml` by default.

### 4. Convert output for the fix engine

The fix engine accepts both YAML and JSON, but JSON is more reliable for large outputs. Convert with `yq`:

```bash
yq -o json output/output.yaml > analysis.json
```

---

## The Fix Engine

The fix engine reads a Konveyor analysis report and applies corrections to your source code. It operates in two phases:

1. **Pattern-based fixes** -- deterministic text replacements (renames, prop removals)
2. **LLM-assisted fixes** -- complex changes delegated to an AI agent

### Previewing changes (dry run)

By default, the fix engine shows a unified diff of what it would change without writing anything:

```bash
frontend-analyzer-provider fix /path/to/project --input analysis.json
```

Output includes:
- A count of pattern-based fixes, LLM-eligible fixes, and manual-review items
- A unified diff preview of all pattern-based edits
- A list of what would be sent to the LLM (if any)

### Applying pattern-based fixes

```bash
frontend-analyzer-provider fix /path/to/project --input analysis.json --apply
```

This writes changes to disk. Pattern-based fixes cover:
- **Component renames** (e.g., `Chip` -> `Label`, `Text` -> `Content`)
- **Prop renames** (e.g., `isActive` -> `isClicked`)
- **Prop removals** (e.g., removed `isHidden` from `AccordionContent`)
- **CSS class prefix changes** (`pf-v5-` -> `pf-v6-`)
- **Prop value changes** (e.g., variant enum values)
- **Import deduplication** (cleans up duplicates after renames)

After applying, the engine reports how many files were modified, edits applied, and edits skipped.

### Filtering by rule

To apply fixes for specific rules only:

```bash
frontend-analyzer-provider fix /path/to/project \
  --input analysis.json \
  --apply \
  --rules pfv6-component-rename-chip,pfv6-css-prefix-v5-to-v6
```

### LLM-assisted fixes

Some violations require structural code changes that can't be expressed as simple find-and-replace. The fix engine supports two LLM backends for these.

#### Using Goose (local AI agent)

[Goose](https://github.com/block/goose) runs as a local CLI agent. It reads your files, applies edits, and writes them directly:

```bash
frontend-analyzer-provider fix /path/to/project \
  --input analysis.json \
  --llm-provider goose \
  --apply
```

Goose groups fixes by file for efficiency. Each request includes the file path, line number, migration rule description, and surrounding code context.

To save prompts and responses for debugging:

```bash
frontend-analyzer-provider fix /path/to/project \
  --input analysis.json \
  --llm-provider goose \
  --apply \
  --log-dir /tmp/goose-logs
```

#### Using an OpenAI-compatible endpoint

For remote LLM services:

```bash
frontend-analyzer-provider fix /path/to/project \
  --input analysis.json \
  --llm-provider openai \
  --llm-endpoint https://your-llm-endpoint/v1/chat/completions \
  --apply
```

The endpoint must be compatible with the OpenAI chat completions API.

### What happens when fixes fail

- If a pattern-based edit can't find the expected text on the target line, it is skipped (reported in the summary).
- If an LLM fix fails (network error, bad response), the incident is moved to **manual review**.
- All manual-review items are listed at the end of the output with file, line number, and rule ID.

Use `--verbose` to see detailed messages and the full description for each manual-review item.

### Recommended workflow

The intended flow is to apply pattern-based fixes first, then re-analyze and run LLM fixes on the remaining violations. This ensures the LLM sees already-renamed code:

```bash
# 1. Analyze
kantra analyze ... --output /tmp/output
yq -o json /tmp/output/output.yaml > analysis.json

# 2. Apply pattern fixes
frontend-analyzer-provider fix ./my-project --input analysis.json --apply

# 3. Re-analyze (pattern-fixed code)
frontend-analyzer-provider analyze ./my-project \
  --rules rules/patternfly-v5-to-v6 \
  --output post-pattern.json \
  --output-format json

# 4. Apply LLM fixes on remaining violations
frontend-analyzer-provider fix ./my-project \
  --input post-pattern.json \
  --llm-provider goose \
  --apply

# 5. Final re-analysis to measure what's left
frontend-analyzer-provider analyze ./my-project \
  --rules rules/patternfly-v5-to-v6 \
  --output final.json \
  --output-format json
```

---

## End-to-End Migration Script

The `hack/run-full-migration.sh` script automates the entire pipeline against a test project ([quipucords-ui](https://github.com/jwmatthews/quipucords-ui)). By default it runs both pattern-based and LLM-assisted fixes:

```bash
# Full pipeline: pattern + LLM fixes (default)
./hack/run-full-migration.sh

# Pattern fixes only, skip LLM
./hack/run-full-migration.sh --no-llm

# Skip the cargo build step
./hack/run-full-migration.sh --skip-build

# Include testing/proxy rules
./hack/run-full-migration.sh --include-testing-rules

# Use an OpenAI-compatible endpoint instead of goose
./hack/run-full-migration.sh --llm-provider=openai

# Save LLM prompts/responses for debugging
./hack/run-full-migration.sh --log-dir=/tmp/llm-logs
```

| Flag | Description |
|---|---|
| `--no-llm` / `--skip-llm` | Skip the LLM fix step (pattern fixes only) |
| `--llm-provider=<name>` | LLM backend to use: `goose` (default) or `openai` |
| `--with-goose` | Backward-compatible alias for `--with-llm` (goose is already the default) |
| `--skip-build` | Skip `cargo build --release` |
| `--include-testing-rules` | Include DOM/CSS/a11y/behavioral proxy rules (excluded by default) |
| `--log-dir=<DIR>` | Save LLM prompts and responses to this directory |

All analysis is done via kantra with the gRPC provider, ensuring full rule engine support (including `not` conditions, `or` combinators, and label selectors). The provider is started once and kept alive for the duration of the pipeline.

The pipeline steps are:

1. **Clone/reset** quipucords-ui to the v5 base commit (`3b3ce52`)
2. **Build** the provider binary (`cargo build --release`)
3. **Start provider + kantra analysis** of the v5 codebase
4. **Pattern fixes** -- deterministic renames, prop removals, CSS prefix changes
5. **LLM fixes** -- re-analyze with kantra after pattern fixes, then send remaining violations to the LLM
6. **Re-analyze with kantra** and print a results report (before/after incident counts, resolved rules, partially fixed rules)
7. **Compare** the automated result against the real human-authored v6 migration

### Results output

The results report breaks down fix effectiveness at each stage:

```
═══════════════════════════════════════════════════════════
  MIGRATION RESULTS
═══════════════════════════════════════════════════════════
  Initial analysis:    150 incidents across 25 rules
  After pattern fixes: 45 incidents (105 fixed, 70%)
  After LLM fixes:     12 incidents (33 fixed, 73% of remaining)
  ─────────────────────────────────────────────────────────
  Total fixed:         138 / 150 incidents (92%)
  Remaining:           12 incidents across 5 rules
  Rules fully resolved: 20 / 25

  Fully resolved rules:
    ✓ pfv6-component-rename-chip (15 incidents)
    ✓ pfv6-css-prefix-v5-to-v6 (42 incidents)
    ...

  Partially fixed rules:
    ◐ pfv6-modal-deprecated: 8 → 3 (5 fixed, 3 remaining)
    ...

  Unfixed rules (no change):
    ○ pfv6-some-complex-rule: 2 incidents
═══════════════════════════════════════════════════════════
```

When `--no-llm` is used, the "After pattern fixes" / "After LLM fixes" breakdown is replaced with a single "After fixes" line.

---

## Comparing Against a Release

After running the migration, use the helper scripts in `hack/` to assess quality against the official upstream v6 migration.

### `compare-against-release.sh`

Compares your automated migration working tree against an official `quipucords-ui` release tag. It isolates only the PF6 migration commit from the upstream release (filtering out post-migration feature work) so the comparison is apples-to-apples.

```bash
# Compare against 2.2.0 (default)
./hack/compare-against-release.sh

# Compare against a different release tag
./hack/compare-against-release.sh --tag 2.3.0

# Include per-file gap details (what specific changes we're missing)
./hack/compare-against-release.sh --gaps

# Write per-file diffs to disk for review
./hack/compare-against-release.sh --diffs

# Write diffs to a custom directory
./hack/compare-against-release.sh --diff-dir /tmp/my-diffs

# Override the migration commit used for comparison
./hack/compare-against-release.sh --migration-commit abc1234

# Point at a custom repo path
./hack/compare-against-release.sh --repo /path/to/quipucords-ui
```

| Flag | Description |
|---|---|
| `--tag <version>` | Release tag to compare against (default: `2.2.0`) |
| `--migration-commit <hash>` | Override the official migration commit (default: `6a8b6a7`) |
| `--repo <path>` | Path to the migration-test repo |
| `--gaps` | Show per-file gap details for partial/different files (categorizes missing changes by type) |
| `--diffs` | Write per-file `.diff` files to `/tmp/quipucords-migration-test/diffs/` |
| `--diff-dir <path>` | Write per-file `.diff` files to a custom directory |

**What the output shows:**

- **File Coverage** -- how many official migration files we touched vs missed
- **Per-File Comparison** -- for overlapping files, rates each as MATCH (identical), PARTIAL (>=70% similar), or DIFFERENT (<70% similar), with similarity percentages
- **Files we missed** -- files the official migration changed that we didn't touch, with change summaries (imports, CSS classes, prop changes, Modal migration, etc.)
- **Extra files** -- files we changed that the official migration didn't

With `--gaps`, the per-file gap analysis categorizes remaining differences: deprecated import paths, Modal API shims, `hasBodyWrapper`, `size="sm"`, `hasAction`, Title wrapping, Icon status wrappers, async test fixes, `popperProps`/`ouiaId`, table props, Avatar, theme classes, align renames, PageSection variant, EmptyState, etc.

With `--diffs`, output is organized into three directories:

```
diffs/
├── missing/   # Files official changed, we didn't (shows official diff)
├── gaps/      # Files both changed (3-way diff: official, ours, delta)
└── extra/     # Files we changed, official didn't (shows our diff)
```

**Prerequisites:** The migration-test repo must exist at `/tmp/quipucords-migration-test/quipucords-ui` (created by `run-full-migration.sh`). The script automatically adds the upstream remote and fetches tags.

### `show-migration-gaps.sh`

Provides a detailed per-file gap analysis between the automated migration and an official release. More focused than `compare-against-release.sh` -- it classifies every file and provides actionable summaries.

```bash
# Full report of all files
./hack/show-migration-gaps.sh

# Only show files with gaps or missing changes (hide matches and extras)
./hack/show-migration-gaps.sh --only-gaps

# Show actual diffs inline
./hack/show-migration-gaps.sh --diff

# Analyze a single file in detail
./hack/show-migration-gaps.sh --file src/app/views/Credentials/CredentialModal.tsx

# Compare against a specific tag
./hack/show-migration-gaps.sh --tag 2.3.0
```

| Flag | Description |
|---|---|
| `--tag <version>` | Release tag to compare against (default: `2.2.0`) |
| `--migration-commit <hash>` | Override the official migration commit (default: `6a8b6a7`) |
| `--file <path>` | Analyze a single file in detail |
| `--only-gaps` | Hide MATCH and EXTRA files, only show GAP and MISSING |
| `--diff` | Show actual diff content inline |
| `--repo <path>` | Path to the migration-test repo |

**File statuses:**

| Status | Meaning |
|--------|---------|
| **MATCH** | Our version is identical to the official migration |
| **GAP** | Both modified the file, but our version differs |
| **MISSING** | Official migration changed this file, but we didn't touch it |
| **EXTRA** | We changed this file, but the official migration didn't |

The summary section includes:

- **Exact match rate** -- percentage of official files we reproduced exactly
- **Coverage rate** -- percentage of official files we at least touched (MATCH + GAP)
- **Action Items** -- lists files to add to migration rules and files needing improvement, with the number of differing lines for each

---

## CLI Reference

```
frontend-analyzer-provider <COMMAND>

Commands:
  analyze    Analyze a project using Konveyor rules
  fix        Apply fixes based on analysis output
  serve      Start as a Konveyor gRPC external provider
```

### `serve`

```
frontend-analyzer-provider serve [OPTIONS]

Options:
  -p, --port <PORT>      TCP port to listen on [default: 9090]
  -s, --socket <PATH>    Unix socket path (alternative to TCP)
```

### `analyze`

```
frontend-analyzer-provider analyze <PROJECT_PATH> [OPTIONS]

Arguments:
  <PROJECT_PATH>         Path to the project to analyze

Options:
  -r, --rules <PATH>         Path to rules directory or YAML file
  -o, --output <PATH>        Output file path [default: stdout]
      --output-format <FMT>  Output format: yaml or json [default: yaml]
```

### `fix`

```
frontend-analyzer-provider fix <PROJECT_PATH> [OPTIONS]

Arguments:
  <PROJECT_PATH>              Path to the project to fix

Options:
  -i, --input <PATH>          Path to analysis output (YAML or JSON)
      --dry-run                Preview changes without writing
      --apply                  Apply changes to disk
      --llm-provider <NAME>   LLM provider: goose or openai
      --llm-endpoint <URL>    LLM endpoint URL (required with openai)
      --rules <IDS>           Only process specific rule IDs (comma-separated)
      --log-dir <DIR>         Directory to save goose logs
  -v, --verbose               Show detailed output
```

---

## Environment Variables

| Variable | Description |
|---|---|
| `RUST_LOG` | Controls log verbosity via `tracing-subscriber`. Default: `info`. Example: `RUST_LOG=debug` |

## License

Apache-2.0
