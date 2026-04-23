//! Top-level JS/TS/JSX/TSX scanner.
//!
//! Walks project files, parses with OXC, and dispatches to capability-specific
//! modules to find matches.

use anyhow::Result;
use frontend_core::capabilities::{ReferenceLocation, ReferencedCondition};
use frontend_core::incident::{extract_code_snip, Incident, Location, Position};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use regex::Regex;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::resolve::ResolverMap;
use crate::transparency::TransparencyCache;

/// Result of scanning: a list of incidents.
pub type ScanResult = Vec<Incident>;

/// A parse error encountered when scanning a file.
#[derive(Debug, Clone)]
pub struct ParseError {
    /// Path of the file that could not be parsed.
    pub file_path: PathBuf,
    /// Human-readable error message from the parser.
    pub message: String,
}

/// Output from a scan that includes both incidents and any parse errors.
#[derive(Debug, Default)]
pub struct ScanOutput {
    /// Incidents found in files that parsed successfully.
    pub incidents: Vec<Incident>,
    /// Files that could not be parsed (OXC fatal errors).
    pub parse_errors: Vec<ParseError>,
}

/// Directories to skip during scanning.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    ".next",
    ".nuxt",
    "coverage",
    "__pycache__",
];

/// File extensions this scanner handles.
const JS_EXTENSIONS: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "mts"];

/// Collect all JS/TS/JSX/TSX files in a project directory.
pub fn collect_files(root: &Path, file_pattern: Option<&str>) -> Result<Vec<PathBuf>> {
    let pattern_re = file_pattern.map(Regex::new).transpose()?;

    let mut files = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(|e| {
        if e.file_type().is_dir() {
            let name = e.file_name().to_string_lossy();
            return !SKIP_DIRS.contains(&name.as_ref());
        }
        true
    }) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let ext = path.extension().unwrap_or_default().to_string_lossy();

        if !JS_EXTENSIONS.contains(&ext.as_ref()) {
            continue;
        }

        // Apply file pattern filter if provided
        if let Some(re) = &pattern_re {
            let path_str = path.to_string_lossy();
            if !re.is_match(&path_str) {
                continue;
            }
        }

        files.push(path.to_path_buf());
    }

    Ok(files)
}

/// Scan a single file for `referenced` condition matches.
///
/// Returns `(incidents, Option<ParseError>)`. When the parser cannot
/// recover, `incidents` will be empty and `parse_error` will describe
/// the failure.
///
/// The `resolver_map` routes each file to the correct `oxc_resolver::Resolver`
/// based on which `tsconfig.json` covers it. Create it once via
/// [`crate::resolve::create_resolver_map`] and reuse across all file scans.
///
/// The `transparency_cache` enables cross-file component resolution:
/// when a JSX parent is a locally-imported wrapper component that passes
/// `{children}` through, the scanner "sees through" it and assigns the
/// grandparent as the effective parent. This prevents false positives on
/// conformance rules that check parent nesting.
///
/// If `file_index` is provided (from the React project index), it will be
/// used for transparency lookups instead of the resolver-based approach.
/// This avoids re-parsing imported files for transparency analysis.
pub fn scan_file_referenced(
    file_path: &Path,
    root: &Path,
    condition: &ReferencedCondition,
    resolver_map: &ResolverMap,
    transparency_cache: &mut TransparencyCache,
    file_index: Option<&crate::index_lookup::FileIndex>,
) -> Result<(ScanResult, Option<ParseError>)> {
    let source = std::fs::read_to_string(file_path)?;
    let source_type = source_type_for_file(file_path, &source);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    if ret.panicked {
        let error_msg = ret
            .errors
            .first()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown parser error".to_string());
        tracing::warn!("Parser panicked on {}: {}", file_path.display(), error_msg);
        return Ok((
            Vec::new(),
            Some(ParseError {
                file_path: file_path.to_path_buf(),
                message: error_msg,
            }),
        ));
    }

    let pattern_re = Regex::new(&condition.pattern)?;
    let file_uri = path_to_uri(file_path, root);
    let mut incidents = Vec::new();

    let location = condition.location.as_ref();

    // Build import map so JSX scanning can resolve components to their
    // import source (e.g., Button → @patternfly/react-core).
    let import_map = crate::imports::build_import_map(&ret.program);

    // Build the set of transparent (children-passthrough) components
    // imported into this file.
    //
    // When a file index is available (from the React project index),
    // transparency info is looked up directly from pre-computed data —
    // no cross-file parsing needed. Otherwise, falls back to the
    // resolver-based approach that parses imported component source files.
    let transparent_components = if let Some(idx) = file_index {
        idx.build_transparency_set(file_path, &import_map)
    } else {
        build_transparency_set(
            file_path,
            &import_map,
            root,
            resolver_map,
            transparency_cache,
        )
    };

    // Compile optional child/notChild/requiresChild regexes for JSX child-matching rules.
    let child_re = condition.child.as_deref().map(Regex::new).transpose()?;
    let not_child_re = condition.not_child.as_deref().map(Regex::new).transpose()?;
    let requires_child_re = condition
        .requires_child
        .as_deref()
        .map(Regex::new)
        .transpose()?;

    // JSX scanning at file level — enables resolving local function calls
    // (e.g., {renderDropdownItems()}) to their bodies for parent context tracing.
    // Uses the resolver for cross-file function reference resolution:
    // when toggle={toggle} passes an imported function as a prop, the scanner
    // follows the import to find the function's JSX body and resolves the parent.
    let resolver = resolver_map.resolver_for_file(file_path);
    match location {
        Some(ReferenceLocation::JsxComponent) | Some(ReferenceLocation::JsxProp) | None => {
            incidents.extend(crate::jsx::scan_jsx_file_with_resolver(
                &ret.program.body,
                &source,
                &pattern_re,
                &file_uri,
                location,
                &import_map,
                child_re.as_ref(),
                not_child_re.as_ref(),
                requires_child_re.as_ref(),
                &transparent_components,
                Some(resolver),
                Some(file_path),
                file_index,
            ));
        }
        _ => {}
    }

    for stmt in &ret.program.body {
        match location {
            Some(ReferenceLocation::Import) | None => {
                incidents.extend(crate::imports::scan_imports(
                    stmt,
                    &source,
                    &pattern_re,
                    &file_uri,
                ));
            }
            _ => {}
        }
        match location {
            Some(ReferenceLocation::FunctionCall) | None => {
                incidents.extend(crate::function_calls::scan_function_calls(
                    stmt,
                    &source,
                    &pattern_re,
                    &file_uri,
                ));
            }
            _ => {}
        }
        match location {
            Some(ReferenceLocation::TypeReference) | None => {
                incidents.extend(crate::type_refs::scan_type_refs(
                    stmt,
                    &source,
                    &pattern_re,
                    &file_uri,
                ));
            }
            _ => {}
        }
    }

    // Filter by component name if specified
    if let Some(component_pattern) = &condition.component {
        let component_re = Regex::new(component_pattern)?;
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(name)) = inc.variables.get("componentName") {
                component_re.is_match(name)
            } else {
                // No componentName variable -- keep it (e.g., import incidents)
                true
            }
        });
    }

    // Filter by parent component name if specified
    if let Some(parent_pattern) = &condition.parent {
        let parent_re = Regex::new(parent_pattern)?;
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(name)) = inc.variables.get("parentName") {
                parent_re.is_match(name)
            } else {
                // No parentName = not a child of any JSX element, filter out
                false
            }
        });
    }

    // Negative parent filter: keep incidents where parent does NOT match.
    // Used for conformance rules like "ModalHeader must be inside Modal" —
    // fires when ModalHeader is used outside a Modal parent.
    if let Some(not_parent_pattern) = &condition.not_parent {
        let not_parent_re = Regex::new(not_parent_pattern)?;
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(name)) = inc.variables.get("parentName") {
                // Render boundaries: the component inside these is composed
                // into the correct parent by the consumer at the call site.
                // Don't fire notParent rules — it's not a real composition
                // violation.
                //
                // - React.Fragment: transparent wrapper, real parent at call site.
                // - __ComponentReturn__: JSX inside a React.FC-typed component
                //   definition; parent determined by the consumer.
                // - __HookReturn__: JSX inside a React hook (use*) body;
                //   always composed into a parent at the call site.
                if name == "React.Fragment" || name.starts_with("__") {
                    return false;
                }
                !not_parent_re.is_match(name)
            } else {
                // No parentName = not inside any JSX parent, keep the incident
                // (component is at the top level, which is wrong for a must-be-in rule)
                true
            }
        });
    }

    // Filter by prop value if specified.
    // Checks both direct propValue (e.g., align="alignRight") and
    // propObjectValues (e.g., align={{ default: "alignRight" }}).
    if let Some(value_pattern) = &condition.value {
        let value_re = Regex::new(value_pattern)?;
        incidents.retain(|inc| {
            // Check direct string value
            if let Some(serde_json::Value::String(val)) = inc.variables.get("propValue") {
                if value_re.is_match(val) {
                    return true;
                }
            }
            // Check object literal string values (responsive breakpoint objects)
            if let Some(serde_json::Value::Array(vals)) = inc.variables.get("propObjectValues") {
                return vals.iter().any(|v| {
                    if let serde_json::Value::String(s) = v {
                        value_re.is_match(s)
                    } else {
                        false
                    }
                });
            }
            // Check function call first argument value
            // e.g., getByRole('button') → callArgValue: "button"
            if let Some(serde_json::Value::String(val)) = inc.variables.get("callArgValue") {
                if value_re.is_match(val) {
                    return true;
                }
            }
            false
        });
    }

    // Filter by import source path if specified.
    // Works for both import incidents (module from import statement) and
    // JSX incidents (module resolved from the file's import map).
    //
    // Uses package-aware matching: exact match OR deep-path match for
    // internal distribution paths (dist/, esm/, etc.). This allows
    // `from: "@patternfly/react-tokens"` to match deep-path imports like
    // `@patternfly/react-tokens/dist/js/chart_color_black_200` while still
    // preventing false matches against logical subpackage exports like
    // `@patternfly/react-core/deprecated`.
    if let Some(from_value) = &condition.from {
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(module)) = inc.variables.get("module") {
                matches_package(module, from_value)
            } else {
                // No module = component not found in imports (e.g., locally
                // defined or HTML element). Keep it to avoid false negatives.
                true
            }
        });
    }

    // Filter by parent import source path if specified.
    // Matches the parent JSX component's import source, resolved from the
    // file's import map. Uses the same package-aware matching as `from`.
    if let Some(parent_from_value) = &condition.parent_from {
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(module)) = inc.variables.get("parentFrom") {
                matches_package(module, parent_from_value)
            } else {
                // No parentFrom = parent not found in imports, filter out
                false
            }
        });
    }

    // Add code snippets
    for incident in &mut incidents {
        incident.code_snip = Some(extract_code_snip(
            &source,
            incident.line_number.unwrap_or(0),
            5,
        ));
    }

    Ok((incidents, None))
}

/// Scan multiple files in parallel using rayon, with index-based cross-file lookups.
///
/// This is the parallel version of the file scanning loop. It requires a
/// `FileIndex` (no mutable transparency cache needed since all cross-file
/// data comes from the pre-built index).
///
/// Returns all incidents and parse errors collected across all files.
pub fn scan_files_parallel(
    files: &[PathBuf],
    root: &Path,
    condition: &ReferencedCondition,
    resolver_map: &ResolverMap,
    file_index: &crate::index_lookup::FileIndex,
) -> Result<(Vec<Incident>, Vec<ParseError>)> {
    use rayon::prelude::*;

    let results: Vec<Result<(ScanResult, Option<ParseError>)>> = files
        .par_iter()
        .map(|file| {
            // Each thread gets its own empty transparency cache (never mutated
            // because the index path is always taken when file_index is Some).
            let mut dummy_cache = TransparencyCache::new();
            scan_file_referenced(
                file,
                root,
                condition,
                resolver_map,
                &mut dummy_cache,
                Some(file_index),
            )
        })
        .collect();

    let mut all_incidents = Vec::new();
    let mut parse_errors = Vec::new();
    let mut errored_files = std::collections::HashSet::new();

    for result in results {
        let (incidents, parse_error) = result?;
        all_incidents.extend(incidents);
        if let Some(err) = parse_error {
            if errored_files.insert(err.file_path.clone()) {
                parse_errors.push(err);
            }
        }
    }

    Ok((all_incidents, parse_errors))
}

/// Build the set of transparent component names for a given file, along with
/// what component each transparent wrapper wraps `{children}` in.
///
/// For each import in the file's import map, resolves the import source to a
/// local file using the resolver selected from `resolver_map` for this file
/// (supporting tsconfig path aliases like `@app/*`). Then analyzes that file
/// (following barrel re-exports) to determine which exported components are
/// children-passthrough wrappers.
///
/// Returns a map of local_name → WrapperInfo:
/// - `None` = pure passthrough (Fragment, div) — collapse to grandparent
/// - `Some("Table")` = wraps children in `<Table>` — substitute as parent
fn build_transparency_set(
    file_path: &Path,
    import_map: &std::collections::HashMap<String, String>,
    root: &Path,
    resolver_map: &ResolverMap,
    cache: &mut TransparencyCache,
) -> std::collections::HashMap<String, crate::transparency::WrapperInfo> {
    let resolver = resolver_map.resolver_for_file(file_path);
    let mut transparent = std::collections::HashMap::new();

    for (local_name, module_source) in import_map {
        // Try to resolve the import using oxc_resolver (handles tsconfig paths)
        let resolved = match crate::resolve::resolve_import_with_resolver(
            resolver,
            file_path,
            module_source,
        ) {
            Some(path) => path,
            None => {
                // Fall back to legacy relative resolver for edge cases
                match crate::resolve::resolve_import(file_path, module_source, root) {
                    Some(path) => path,
                    None => continue, // npm package or unresolvable — skip
                }
            }
        };

        // Skip npm packages (resolved to node_modules)
        if crate::resolve::is_node_modules_path(&resolved) {
            continue;
        }

        // Analyze transparency with barrel re-export following
        let file_transparency = crate::transparency::analyze_file_transparency_with_resolver(
            &resolved, resolver, cache, 0,
        )
        .unwrap_or_default();

        // The import map maps local_name → module_source.
        // The transparency analysis returns component names as declared in the source file.
        // For named imports (`import { Foo } from './Foo'`), the local name and
        // exported name may differ (aliasing). We check both the local name and
        // whether the local name matches any transparent export.
        if let Some(wrapper_info) = file_transparency.get(local_name) {
            transparent.insert(local_name.clone(), wrapper_info.clone());
        }
    }

    transparent
}

/// Scan a single file for CSS class name references in JS/TS (className attributes, etc.).
pub fn scan_file_classnames(
    file_path: &Path,
    root: &Path,
    pattern: &Regex,
) -> Result<(ScanResult, Option<ParseError>)> {
    let source = std::fs::read_to_string(file_path)?;
    let source_type = source_type_for_file(file_path, &source);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    if ret.panicked {
        let error_msg = ret
            .errors
            .first()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown parser error".to_string());
        tracing::warn!("Parser panicked on {}: {}", file_path.display(), error_msg);
        return Ok((
            Vec::new(),
            Some(ParseError {
                file_path: file_path.to_path_buf(),
                message: error_msg,
            }),
        ));
    }

    let file_uri = path_to_uri(file_path, root);
    let mut incidents = Vec::new();

    for stmt in &ret.program.body {
        incidents.extend(crate::classnames::scan_classname_usage(
            stmt, &source, pattern, &file_uri,
        ));
    }

    for incident in &mut incidents {
        incident.code_snip = Some(extract_code_snip(
            &source,
            incident.line_number.unwrap_or(0),
            5,
        ));
    }

    Ok((incidents, None))
}

/// Scan a single file for CSS variable references in JS/TS.
pub fn scan_file_css_vars(
    file_path: &Path,
    root: &Path,
    pattern: &Regex,
) -> Result<(ScanResult, Option<ParseError>)> {
    let source = std::fs::read_to_string(file_path)?;
    let source_type = source_type_for_file(file_path, &source);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    if ret.panicked {
        let error_msg = ret
            .errors
            .first()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown parser error".to_string());
        tracing::warn!("Parser panicked on {}: {}", file_path.display(), error_msg);
        return Ok((
            Vec::new(),
            Some(ParseError {
                file_path: file_path.to_path_buf(),
                message: error_msg,
            }),
        ));
    }

    let file_uri = path_to_uri(file_path, root);
    let mut incidents = Vec::new();

    for stmt in &ret.program.body {
        incidents.extend(crate::css_vars::scan_css_var_usage(
            stmt, &source, pattern, &file_uri,
        ));
    }

    for incident in &mut incidents {
        incident.code_snip = Some(extract_code_snip(
            &source,
            incident.line_number.unwrap_or(0),
            5,
        ));
    }

    Ok((incidents, None))
}

/// Determine the OXC SourceType from a file path and source content.
///
/// Always enables JSX since it's a superset of JS and won't cause false
/// positives on non-JSX files. Detects CJS vs ESM by checking for
/// `require(` / `module.exports` patterns in the source.
fn source_type_for_file(path: &Path, source: &str) -> SourceType {
    let ext = path.extension().unwrap_or_default().to_string_lossy();

    let base = match ext.as_ref() {
        "tsx" => return SourceType::tsx(),
        "ts" | "mts" => return SourceType::ts(),
        "jsx" => return SourceType::jsx(),
        "cjs" => return SourceType::cjs().with_jsx(true),
        "mjs" => return SourceType::mjs().with_jsx(true),
        // For .js files, detect CJS vs ESM from content
        "js" => {
            let has_import = source.contains("import ")
                && (source.contains(" from ") || source.contains("import {"));
            let has_require = source.contains("require(") || source.contains("module.exports");

            if has_import {
                SourceType::mjs()
            } else if has_require {
                SourceType::cjs()
            } else {
                // Default to ESM for unknown .js files
                SourceType::mjs()
            }
        }
        _ => SourceType::mjs(),
    };

    base.with_jsx(true)
}

/// Convert a file path to a file:// URI.
pub fn path_to_uri(path: &Path, root: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    format!("file://{}", absolute.display())
}

/// Compute 1-indexed line number from a byte offset in source text.
pub fn line_number_from_offset(source: &str, offset: u32) -> u32 {
    let clamped = (offset as usize).min(source.len());
    source[..clamped].chars().filter(|c| *c == '\n').count() as u32 + 1
}

/// Create an `Incident` from source location info.
pub fn make_incident(source: &str, file_uri: &str, start_offset: u32, end_offset: u32) -> Incident {
    let start_clamped = (start_offset as usize).min(source.len());
    let end_clamped = (end_offset as usize).min(source.len());

    let line = line_number_from_offset(source, start_offset);
    let start_col = source[..start_clamped]
        .rfind('\n')
        .map(|p| start_clamped - p - 1)
        .unwrap_or(start_clamped) as u32;
    let end_col = source[..end_clamped]
        .rfind('\n')
        .map(|p| end_clamped - p - 1)
        .unwrap_or(end_clamped) as u32;
    let end_line = line_number_from_offset(source, end_offset);

    Incident::new(
        file_uri.to_string(),
        line,
        Location {
            start: Position {
                line: line - 1, // 0-indexed for LSP compatibility
                character: start_col,
            },
            end: Position {
                line: end_line - 1,
                character: end_col,
            },
        },
    )
}

/// Check if an import module path matches a package name, accounting for
/// deep-path imports into the package's internal distribution files.
///
/// Deep-path imports like `@patternfly/react-tokens/dist/js/chart_color_black_200`
/// resolve to files within the same package and should match the bare package
/// name `@patternfly/react-tokens`.
///
/// Logical subpackage exports like `@patternfly/react-core/deprecated` are
/// different API surfaces and must NOT match the bare package name.
///
/// Examples:
/// ```text
/// matches_package("@patternfly/react-tokens/dist/js/foo", "@patternfly/react-tokens") → true
/// matches_package("@patternfly/react-core/deprecated",    "@patternfly/react-core")   → false
/// matches_package("@patternfly/react-core",               "@patternfly/react-core")   → true
/// ```
fn matches_package(module: &str, package: &str) -> bool {
    if module == package {
        return true;
    }
    // Deep-path imports resolve to files within the package's build output.
    // These are the same package, just a direct entry point into an internal
    // file. Subpackage exports like /deprecated and /next are logical
    // sub-packages and must NOT match the bare package name.
    if let Some(rest) = module
        .strip_prefix(package)
        .and_then(|s| s.strip_prefix('/'))
    {
        let internal_prefixes = ["dist/", "es/", "esm/", "cjs/", "lib/", "src/", "build/"];
        internal_prefixes.iter().any(|p| rest.starts_with(p))
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── line_number_from_offset tests ────────────────────────────────────

    #[test]
    fn test_line_number_offset_zero() {
        assert_eq!(line_number_from_offset("hello\nworld", 0), 1);
    }

    #[test]
    fn test_line_number_first_line() {
        assert_eq!(line_number_from_offset("hello\nworld", 3), 1);
    }

    #[test]
    fn test_line_number_at_newline() {
        // offset 5 is the '\n' character
        assert_eq!(line_number_from_offset("hello\nworld", 5), 1);
    }

    #[test]
    fn test_line_number_second_line() {
        assert_eq!(line_number_from_offset("hello\nworld", 6), 2);
    }

    #[test]
    fn test_line_number_third_line() {
        assert_eq!(line_number_from_offset("a\nb\nc\nd", 4), 3);
    }

    #[test]
    fn test_line_number_single_line() {
        assert_eq!(line_number_from_offset("no newlines", 5), 1);
    }

    #[test]
    fn test_line_number_offset_beyond_source() {
        // Should clamp to source length
        assert_eq!(line_number_from_offset("a\nb", 999), 2);
    }

    #[test]
    fn test_line_number_empty_source() {
        assert_eq!(line_number_from_offset("", 0), 1);
    }

    // ── path_to_uri tests ────────────────────────────────────────────────

    #[test]
    fn test_path_to_uri_absolute() {
        let uri = path_to_uri(Path::new("/home/user/src/App.tsx"), Path::new("/root"));
        assert_eq!(uri, "file:///home/user/src/App.tsx");
    }

    #[test]
    fn test_path_to_uri_relative() {
        let uri = path_to_uri(Path::new("src/App.tsx"), Path::new("/home/user/project"));
        assert_eq!(uri, "file:///home/user/project/src/App.tsx");
    }

    // ── make_incident tests ──────────────────────────────────────────────

    #[test]
    fn test_make_incident_basic() {
        let source = "import { Button } from '@patternfly/react-core';";
        let incident = make_incident(source, "file:///test.tsx", 0, 48);
        assert_eq!(incident.file_uri, "file:///test.tsx");
        assert_eq!(incident.line_number, Some(1));
        let loc = incident.code_location.unwrap();
        assert_eq!(loc.start.line, 0); // 0-indexed
        assert_eq!(loc.start.character, 0);
    }

    #[test]
    fn test_make_incident_second_line() {
        let source = "line one\nimport { X } from 'y';";
        // "import" starts at offset 9
        let incident = make_incident(source, "file:///test.tsx", 9, 30);
        assert_eq!(incident.line_number, Some(2));
        let loc = incident.code_location.unwrap();
        assert_eq!(loc.start.line, 1); // 0-indexed
        assert_eq!(loc.start.character, 0);
    }

    #[test]
    fn test_make_incident_column_calculation() {
        let source = "  const x = 1;";
        // "x" is at offset 8
        let incident = make_incident(source, "file:///test.tsx", 8, 9);
        assert_eq!(incident.line_number, Some(1));
        let loc = incident.code_location.unwrap();
        assert_eq!(loc.start.character, 8);
        assert_eq!(loc.end.character, 9);
    }

    // ── source_type_for_file tests ───────────────────────────────────────

    #[test]
    fn test_source_type_tsx() {
        let st = source_type_for_file(Path::new("app.tsx"), "");
        assert!(st.is_typescript());
        assert!(st.is_jsx());
    }

    #[test]
    fn test_source_type_ts() {
        let st = source_type_for_file(Path::new("app.ts"), "");
        assert!(st.is_typescript());
    }

    #[test]
    fn test_source_type_jsx() {
        let st = source_type_for_file(Path::new("app.jsx"), "");
        assert!(st.is_jsx());
    }

    #[test]
    fn test_source_type_js_with_import() {
        let st = source_type_for_file(Path::new("app.js"), "import { foo } from 'bar';");
        assert!(st.is_jsx()); // JSX always enabled
    }

    #[test]
    fn test_source_type_js_with_require() {
        let st = source_type_for_file(Path::new("app.js"), "const foo = require('bar');");
        assert!(st.is_jsx()); // JSX always enabled
    }

    #[test]
    fn test_source_type_cjs() {
        let st = source_type_for_file(Path::new("app.cjs"), "");
        assert!(st.is_jsx());
    }

    #[test]
    fn test_source_type_mjs() {
        let st = source_type_for_file(Path::new("app.mjs"), "");
        assert!(st.is_jsx());
    }

    // ── parse error reporting tests ──────────────────────────────────────

    #[test]
    fn test_scan_file_referenced_returns_parse_error_for_broken_syntax() {
        use frontend_core::capabilities::ReferencedCondition;

        // Create a temp file with broken import syntax (@ in import binding)
        let dir = std::env::temp_dir().join("scanner_test_parse_error");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("broken.tsx");
        std::fs::write(
            &file_path,
            "import { @invalid/path } from \"some-package\";\n",
        )
        .unwrap();

        let condition = ReferencedCondition {
            pattern: ".*".to_string(),
            location: None,
            component: None,
            parent: None,
            value: None,
            from: None,
            parent_from: None,
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
            not_parent: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, parse_error) =
            scan_file_referenced(&file_path, &dir, &condition, &resolver_map, &mut cache, None).unwrap();

        // Should return no incidents (parser couldn't produce an AST)
        assert!(incidents.is_empty());
        // Should return a parse error with the file path and a message
        assert!(
            parse_error.is_some(),
            "Expected parse error for broken syntax"
        );
        let err = parse_error.unwrap();
        assert_eq!(err.file_path, file_path);
        assert!(
            !err.message.is_empty(),
            "Parse error message should not be empty"
        );

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_file_referenced_no_parse_error_for_valid_syntax() {
        use frontend_core::capabilities::ReferencedCondition;

        let dir = std::env::temp_dir().join("scanner_test_no_parse_error");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("valid.tsx");
        std::fs::write(
            &file_path,
            "import { Button } from '@patternfly/react-core';\n",
        )
        .unwrap();

        let condition = ReferencedCondition {
            pattern: "^Button$".to_string(),
            location: None,
            component: None,
            parent: None,
            value: None,
            from: None,
            parent_from: None,
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
            not_parent: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, parse_error) =
            scan_file_referenced(&file_path, &dir, &condition, &resolver_map, &mut cache, None).unwrap();

        // Valid file should have no parse error
        assert!(
            parse_error.is_none(),
            "Valid file should not produce a parse error"
        );
        // Should find the import
        assert!(!incidents.is_empty());

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Transparent wrapper integration tests ────────────────────────────

    /// Helper: create a project with multiple files and scan one of them.
    /// Returns the incidents found by `scan_file_referenced`.
    fn scan_with_wrapper_project(
        wrapper_source: &str,
        consumer_source: &str,
        condition: &ReferencedCondition,
    ) -> Vec<frontend_core::incident::Incident> {
        let dir = std::env::temp_dir().join(format!(
            "scanner_transparency_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        let components_dir = src_dir.join("components");
        std::fs::create_dir_all(&components_dir).unwrap();

        // Write the wrapper component file
        std::fs::write(
            components_dir.join("ConditionalTableBody.tsx"),
            wrapper_source,
        )
        .unwrap();

        // Write the consumer file
        let consumer_path = src_dir.join("App.tsx");
        std::fs::write(&consumer_path, consumer_source).unwrap();

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&consumer_path, &dir, condition, &resolver_map, &mut cache, None)
                .unwrap();

        std::fs::remove_dir_all(&dir).ok();
        incidents
    }

    #[test]
    fn test_transparent_wrapper_tbody_gets_table_parent() {
        // Simulates the real ConditionalTableBody pattern:
        // <Table><ConditionalTableBody><Tbody/></ConditionalTableBody></Table>
        // Tbody should see parentName = "Table", not "ConditionalTableBody"
        let wrapper = r#"
import React from 'react';
import { Tbody, Tr, Td } from '@patternfly/react-table';

export const ConditionalTableBody = ({
    isLoading,
    isError,
    children
}) => (
    <React.Fragment>
        {isLoading ? (
            <Tbody><Tr><Td>Loading...</Td></Tr></Tbody>
        ) : isError ? (
            <Tbody><Tr><Td>Error</Td></Tr></Tbody>
        ) : (
            children
        )}
    </React.Fragment>
);
"#;

        let consumer = r#"
import { Table, Thead, Tbody, Tr, Th, Td } from '@patternfly/react-table';
import { ConditionalTableBody } from './components/ConditionalTableBody';

const App = () => (
    <Table aria-label="Example table">
        <Thead>
            <Tr><Th>Name</Th></Tr>
        </Thead>
        <ConditionalTableBody isLoading={false} isError={false} numRenderedColumns={1}>
            <Tbody>
                <Tr><Td>Row 1</Td></Tr>
            </Tbody>
        </ConditionalTableBody>
    </Table>
);
"#;

        // Rule: find Tbody, check its parent
        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
            not_parent: None,
        };

        let incidents = scan_with_wrapper_project(wrapper, consumer, &condition);

        // Tbody inside ConditionalTableBody should have parentName="Table"
        // because ConditionalTableBody is transparent, so the parent filter
        // `parent: ^Table$` should match.
        assert!(
            !incidents.is_empty(),
            "Tbody should match with parent=Table (transparent wrapper collapsed)"
        );

        // Verify the parentName is actually "Table"
        let tbody_incident = incidents
            .iter()
            .find(|i| i.variables.get("componentName").and_then(|v| v.as_str()) == Some("Tbody"))
            .expect("Should have a Tbody incident");

        assert_eq!(
            tbody_incident
                .variables
                .get("parentName")
                .and_then(|v| v.as_str()),
            Some("Table"),
            "Tbody's parentName should be 'Table', not 'ConditionalTableBody'"
        );
    }

    #[test]
    fn test_transparent_wrapper_not_parent_rule_no_false_positive() {
        // Simulates: conformance rule "Tbody must be inside Table"
        // using notParent: ^Table$. With the wrapper collapsed,
        // the rule should NOT fire (Tbody IS inside Table).
        let wrapper = r#"
import React from 'react';
export const ConditionalTableBody = ({ children, isLoading }) => (
    <React.Fragment>
        {isLoading ? <div>Loading</div> : children}
    </React.Fragment>
);
"#;

        let consumer = r#"
import { Table, Tbody, Tr, Td } from '@patternfly/react-table';
import { ConditionalTableBody } from './components/ConditionalTableBody';

const App = () => (
    <Table>
        <ConditionalTableBody isLoading={false}>
            <Tbody><Tr><Td>Data</Td></Tr></Tbody>
        </ConditionalTableBody>
    </Table>
);
"#;

        // Conformance rule: Tbody must be inside Table (fire when NOT inside Table)
        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents = scan_with_wrapper_project(wrapper, consumer, &condition);

        // Should find NO incidents: Tbody IS inside Table (wrapper is transparent)
        assert!(
            incidents.is_empty(),
            "notParent rule should not fire — Tbody is inside Table (wrapper collapsed). Got {} incidents",
            incidents.len()
        );
    }

    #[test]
    fn test_opaque_component_preserves_parent() {
        // A component that does NOT pass children through should still
        // become the parent for its children (existing behavior).
        let wrapper = r#"
import React from 'react';
export const OpaqueWrapper = ({ title }) => (
    <div>
        <h1>{title}</h1>
        <p>No children rendering here</p>
    </div>
);
"#;

        let consumer = r#"
import { Table, Tbody } from '@patternfly/react-table';
import { OpaqueWrapper } from './components/ConditionalTableBody';

const App = () => (
    <Table>
        <OpaqueWrapper title="hello">
            <Tbody />
        </OpaqueWrapper>
    </Table>
);
"#;

        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
            not_parent: None,
        };

        let incidents = scan_with_wrapper_project(wrapper, consumer, &condition);

        // OpaqueWrapper is NOT transparent, so Tbody should have
        // parentName="OpaqueWrapper", not "Table". The parent filter
        // `parent: ^Table$` should NOT match.
        assert!(
            incidents.is_empty(),
            "Tbody inside opaque wrapper should NOT match parent=Table"
        );
    }

    #[test]
    fn test_npm_package_component_not_resolved() {
        // Components from npm packages should NOT be treated as transparent,
        // even if they happen to pass children through. Only local imports
        // are resolved.
        let consumer = r#"
import { Table, Tbody } from '@patternfly/react-table';
import { Bullseye } from '@patternfly/react-core';

const App = () => (
    <Table>
        <Bullseye>
            <Tbody />
        </Bullseye>
    </Table>
);
"#;

        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: Some("^Bullseye$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
            not_parent: None,
        };

        // For this test we don't need a wrapper file since Bullseye is from npm
        let dir = std::env::temp_dir().join(format!(
            "scanner_npm_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let consumer_path = src_dir.join("App.tsx");
        std::fs::write(&consumer_path, consumer).unwrap();

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&consumer_path, &dir, &condition, &resolver_map, &mut cache, None)
                .unwrap();

        // Tbody should have parentName="Bullseye" (npm component stays opaque)
        assert!(
            !incidents.is_empty(),
            "Tbody should match with parent=Bullseye (npm component is opaque)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_not_parent_suppressed_for_react_fragment() {
        // Components rendered inside React.Fragment are at a render boundary —
        // the real parent is supplied by the consumer. notParent rules should
        // not fire for these.
        use frontend_core::capabilities::ReferencedCondition;

        let dir = std::env::temp_dir().join(format!(
            "scanner_fragment_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        // Simulates ConditionalTableBody.tsx: renders <Tbody> inside <React.Fragment>
        let source = r#"
import React from 'react';
import { Tbody, Tr, Td } from '@patternfly/react-table';

export const ConditionalTableBody = ({ isLoading, children }) => (
    <React.Fragment>
        {isLoading ? (
            <Tbody>
                <Tr><Td>Loading...</Td></Tr>
            </Tbody>
        ) : (
            children
        )}
    </React.Fragment>
);
"#;
        let file_path = src_dir.join("ConditionalTableBody.tsx");
        std::fs::write(&file_path, source).unwrap();

        // Conformance rule: Tbody must be inside Table (fire when NOT inside Table)
        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&file_path, &dir, &condition, &resolver_map, &mut cache, None).unwrap();

        // Tbody inside React.Fragment should NOT trigger — it's a render boundary
        assert!(
            incidents.is_empty(),
            "notParent rule should not fire when parent is React.Fragment (render boundary). Got {} incidents",
            incidents.len()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_not_parent_still_fires_for_wrong_parent() {
        // When the parent IS a real component (not Fragment) and doesn't match,
        // the rule should still fire.
        use frontend_core::capabilities::ReferencedCondition;

        let dir = std::env::temp_dir().join(format!(
            "scanner_wrong_parent_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let source = r#"
import { Tbody, Tr } from '@patternfly/react-table';
import { Card } from '@patternfly/react-core';

const App = () => (
    <Card>
        <Tbody>
            <Tr />
        </Tbody>
    </Card>
);
"#;
        let file_path = src_dir.join("App.tsx");
        std::fs::write(&file_path, source).unwrap();

        // Tbody must be inside Table — Card is wrong
        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&file_path, &dir, &condition, &resolver_map, &mut cache, None).unwrap();

        // Tbody inside Card should fire — Card is not Table
        assert!(
            !incidents.is_empty(),
            "notParent rule should fire when parent is a real wrong component"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_not_parent_suppressed_for_hook_returning_jsx() {
        // A hook returning JSX directly: the component has no real parent,
        // but it's a render boundary — notParent should NOT fire.
        use frontend_core::capabilities::ReferencedCondition;

        let dir = std::env::temp_dir().join(format!(
            "scanner_hook_return_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';

export function useToolbarActions() {
    return <ToolbarItem>Action</ToolbarItem>;
}
"#;
        let file_path = src_dir.join("useToolbarActions.tsx");
        std::fs::write(&file_path, source).unwrap();

        // Conformance rule: ToolbarItem must be inside Toolbar
        let condition = ReferencedCondition {
            pattern: "^ToolbarItem$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Toolbar$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&file_path, &dir, &condition, &resolver_map, &mut cache, None).unwrap();

        // ToolbarItem inside a hook is a render boundary — should NOT fire
        assert!(
            incidents.is_empty(),
            "notParent should not fire for JSX inside a hook (render boundary). Got {} incidents: {:?}",
            incidents.len(),
            incidents.iter().map(|i| &i.variables).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_not_parent_suppressed_for_react_fc_component() {
        // A React.FC typed component: the real parent is at the call site.
        use frontend_core::capabilities::ReferencedCondition;

        let dir = std::env::temp_dir().join(format!(
            "scanner_react_fc_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let source = r#"
import React from 'react';
import { ToolbarItem } from '@patternfly/react-core';

export const ToolbarActions: React.FC = () => (
    <ToolbarItem>Action</ToolbarItem>
);
"#;
        let file_path = src_dir.join("ToolbarActions.tsx");
        std::fs::write(&file_path, source).unwrap();

        let condition = ReferencedCondition {
            pattern: "^ToolbarItem$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Toolbar$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&file_path, &dir, &condition, &resolver_map, &mut cache, None).unwrap();

        assert!(
            incidents.is_empty(),
            "notParent should not fire for JSX inside a React.FC component. Got {} incidents: {:?}",
            incidents.len(),
            incidents.iter().map(|i| &i.variables).collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_not_parent_still_fires_for_wrong_parent_inside_hook() {
        // Even inside a hook, if a component has a REAL wrong parent,
        // the rule should still fire.
        use frontend_core::capabilities::ReferencedCondition;

        let dir = std::env::temp_dir().join(format!(
            "scanner_hook_wrong_parent_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let source = r#"
import { Card } from '@patternfly/react-core';
import { ToolbarItem } from '@patternfly/react-core';

export function useMyHook() {
    return (
        <Card>
            <ToolbarItem>Action</ToolbarItem>
        </Card>
    );
}
"#;
        let file_path = src_dir.join("useMyHook.tsx");
        std::fs::write(&file_path, source).unwrap();

        let condition = ReferencedCondition {
            pattern: "^ToolbarItem$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Toolbar$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&file_path, &dir, &condition, &resolver_map, &mut cache, None).unwrap();

        // ToolbarItem inside Card (wrong parent) should still fire
        assert!(
            !incidents.is_empty(),
            "notParent should fire when JSX has a real wrong parent inside a hook"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── tsconfig path alias + barrel re-export integration tests ─────────
    //
    // These tests simulate the tackle2-ui patterns that produce false
    // positives: components imported via tsconfig path aliases (@app/*)
    // through barrel files (index.ts with `export * from './X'`).

    /// Helper: create a project with tsconfig path aliases, barrel files,
    /// and multiple component files. Scans the consumer file and returns
    /// incidents.
    fn scan_with_tsconfig_project(
        files: &[(&str, &str)], // (relative path, content)
        consumer_rel_path: &str,
        tsconfig_content: &str,
        condition: &ReferencedCondition,
    ) -> Vec<frontend_core::incident::Incident> {
        let dir = std::env::temp_dir().join(format!(
            "scanner_tsconfig_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // Write tsconfig.json
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tsconfig.json"), tsconfig_content).unwrap();

        // Write all files
        for (rel_path, content) in files {
            let full_path = dir.join(rel_path);
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full_path, content).unwrap();
        }

        let consumer_path = dir.join(consumer_rel_path);
        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&consumer_path, &dir, condition, &resolver_map, &mut cache, None)
                .unwrap();

        std::fs::remove_dir_all(&dir).ok();
        incidents
    }

    #[test]
    fn test_tsconfig_path_alias_transparent_wrapper() {
        // Simulates: TableHeaderContentWithControls imported via @app/ alias
        // Pattern: <Tr><TableHeaderContentWithControls><Th/></...></Tr>
        // TableHeaderContentWithControls is a Fragment wrapper — transparent.
        // Without tsconfig resolution, Th gets parentName="TableHeaderContentWithControls"
        // and notParent: ^Tr$ fires (false positive).
        // With tsconfig resolution, Th should get parentName="Tr" (no incident).

        let tsconfig = r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@app/*": ["src/app/*"]
                }
            }
        }"#;

        let wrapper = r#"
import * as React from 'react';
import { Th } from '@patternfly/react-table';

export const TableHeaderContentWithControls = ({
    numColumnsBeforeData,
    numColumnsAfterData,
    children
}) => (
    <>
        {Array(numColumnsBeforeData).fill(null).map((_, i) => (
            <Th key={i} />
        ))}
        {children}
        {Array(numColumnsAfterData).fill(null).map((_, i) => (
            <Th key={i} />
        ))}
    </>
);
"#;

        let consumer = r#"
import { Table, Thead, Tbody, Tr, Th, Td } from '@patternfly/react-table';
import { TableHeaderContentWithControls } from '@app/components/TableControls';

const App = () => (
    <Table>
        <Thead>
            <Tr>
                <TableHeaderContentWithControls numColumnsBeforeData={1} numColumnsAfterData={0}>
                    <Th>Name</Th>
                    <Th>Status</Th>
                </TableHeaderContentWithControls>
            </Tr>
        </Thead>
    </Table>
);
"#;

        // Barrel file that re-exports
        let barrel = r#"export * from './TableHeaderContentWithControls';"#;

        let files = &[
            (
                "src/app/components/TableControls/TableHeaderContentWithControls.tsx",
                wrapper,
            ),
            ("src/app/components/TableControls/index.ts", barrel),
            ("src/app/pages/App.tsx", consumer),
        ];

        // Rule: Th must be in Tr (fire when NOT in Tr)
        let condition = ReferencedCondition {
            pattern: "^Th$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Tr$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents =
            scan_with_tsconfig_project(files, "src/app/pages/App.tsx", tsconfig, &condition);

        // With transparent wrapper resolution via @app/ alias + barrel file,
        // Th should have parentName="Tr" (wrapper collapsed), so notParent
        // rule should NOT fire.
        assert!(
            incidents.is_empty(),
            "notParent: ^Tr$ should NOT fire — Th is inside Tr \
             (TableHeaderContentWithControls is transparent, imported via @app/ alias). \
             Got {} incidents with parents: {:?}",
            incidents.len(),
            incidents
                .iter()
                .map(|i| i
                    .variables
                    .get("parentName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_tsconfig_barrel_file_conditional_table_body() {
        // Simulates: ConditionalTableBody imported via @app/ through barrel
        // Pattern: <Table><ConditionalTableBody><Tbody/></...></Table>
        // ConditionalTableBody is transparent — passes {children} in happy path.

        let tsconfig = r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@app/*": ["src/app/*"]
                }
            }
        }"#;

        let wrapper = r#"
import React from 'react';
import { Tbody, Tr, Td } from '@patternfly/react-table';

export const ConditionalTableBody = ({
    isLoading,
    isError,
    children
}) => (
    <React.Fragment>
        {isLoading ? (
            <Tbody><Tr><Td>Loading...</Td></Tr></Tbody>
        ) : isError ? (
            <Tbody><Tr><Td>Error</Td></Tr></Tbody>
        ) : (
            children
        )}
    </React.Fragment>
);
"#;

        let consumer = r#"
import { Table, Tbody, Tr, Td } from '@patternfly/react-table';
import { ConditionalTableBody } from '@app/components/TableControls';

const App = () => (
    <Table>
        <ConditionalTableBody isLoading={false} isError={false}>
            <Tbody>
                <Tr><Td>Row 1</Td></Tr>
            </Tbody>
        </ConditionalTableBody>
    </Table>
);
"#;

        let barrel = r#"
export * from './TableHeaderContentWithControls';
export * from './ConditionalTableBody';
"#;

        // Need the other file too for barrel to parse
        let dummy_wrapper = r#"
export const TableHeaderContentWithControls = ({ children }) => <>{children}</>;
"#;

        let files = &[
            (
                "src/app/components/TableControls/ConditionalTableBody.tsx",
                wrapper,
            ),
            (
                "src/app/components/TableControls/TableHeaderContentWithControls.tsx",
                dummy_wrapper,
            ),
            ("src/app/components/TableControls/index.ts", barrel),
            ("src/app/pages/App.tsx", consumer),
        ];

        // Rule: Tbody must be inside Table (fire when NOT inside Table)
        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents =
            scan_with_tsconfig_project(files, "src/app/pages/App.tsx", tsconfig, &condition);

        // ConditionalTableBody is transparent, so Tbody should see Table as parent.
        // The notParent: ^Table$ rule should NOT fire.
        assert!(
            incidents.is_empty(),
            "notParent: ^Table$ should NOT fire — Tbody is inside Table \
             (ConditionalTableBody is transparent via @app/ barrel import). \
             Got {} incidents",
            incidents.len()
        );
    }

    #[test]
    fn test_tsconfig_rbac_transparent_wrapper() {
        // Simulates: RBAC component imported via @app/rbac
        // RBAC is a pure logic gate: returns children or false (no DOM node).
        // Pattern: <Toolbar><RBAC><ToolbarItem/></RBAC></Toolbar>

        let tsconfig = r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@app/*": ["src/app/*"]
                }
            }
        }"#;

        let rbac = r#"
export const RBAC = ({ allowedPermissions, children }) => {
    const hasAccess = true;
    return hasAccess && children;
};
"#;

        let consumer = r#"
import { Toolbar, ToolbarContent, ToolbarItem } from '@patternfly/react-core';
import { RBAC } from '@app/rbac';

const App = () => (
    <Toolbar>
        <ToolbarContent>
            <RBAC allowedPermissions={['write']}>
                <ToolbarItem>Action</ToolbarItem>
            </RBAC>
        </ToolbarContent>
    </Toolbar>
);
"#;

        let files = &[
            ("src/app/rbac.ts", rbac),
            ("src/app/pages/App.tsx", consumer),
        ];

        // Rule: ToolbarItem must be in Toolbar (fire when NOT in Toolbar)
        let condition = ReferencedCondition {
            pattern: "^ToolbarItem$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Toolbar$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents =
            scan_with_tsconfig_project(files, "src/app/pages/App.tsx", tsconfig, &condition);

        // RBAC is transparent (returns children directly), so ToolbarItem
        // should see ToolbarContent as parent (not RBAC). ToolbarContent
        // is from @patternfly (opaque), so ToolbarItem's parent = ToolbarContent.
        // The notParent: ^Toolbar$ rule should still fire because the direct
        // PF parent is ToolbarContent, not Toolbar. But critically, it should
        // NOT report RBAC as the parent — RBAC should be collapsed out.
        for incident in &incidents {
            let parent = incident
                .variables
                .get("parentName")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            assert_ne!(
                parent, "RBAC",
                "RBAC should be collapsed as transparent — parent should not be RBAC"
            );
        }
    }

    #[test]
    fn test_tsconfig_opaque_wrapper_not_collapsed() {
        // Verify that a non-transparent component imported via @app/ is NOT
        // collapsed — it should remain as the parent.

        let tsconfig = r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@app/*": ["src/app/*"]
                }
            }
        }"#;

        // This wrapper renders its OWN content, does not pass children
        let opaque = r#"
export const OpaqueWrapper = ({ title }) => (
    <div>
        <h1>{title}</h1>
    </div>
);
"#;

        let consumer = r#"
import { Table, Tbody } from '@patternfly/react-table';
import { OpaqueWrapper } from '@app/components/OpaqueWrapper';

const App = () => (
    <Table>
        <OpaqueWrapper title="hello">
            <Tbody />
        </OpaqueWrapper>
    </Table>
);
"#;

        let files = &[
            ("src/app/components/OpaqueWrapper.tsx", opaque),
            ("src/app/pages/App.tsx", consumer),
        ];

        // Rule: Tbody must be in Table (parent filter, not notParent)
        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
            not_parent: None,
        };

        let incidents =
            scan_with_tsconfig_project(files, "src/app/pages/App.tsx", tsconfig, &condition);

        // OpaqueWrapper is NOT transparent, so Tbody should have
        // parentName="OpaqueWrapper", not "Table". The parent=Table
        // filter should NOT match.
        assert!(
            incidents.is_empty(),
            "Tbody inside opaque @app/ wrapper should NOT match parent=Table. \
             Got {} incidents",
            incidents.len()
        );
    }

    #[test]
    fn test_tsconfig_named_reexport_barrel() {
        // Test that named re-exports (`export { Foo } from './X'`) work
        // through barrel files.

        let tsconfig = r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@app/*": ["src/app/*"]
                }
            }
        }"#;

        let wrapper = r#"
export const MyWrapper = ({ children }) => <>{children}</>;
export const MyOpaqueComponent = ({ text }) => <div>{text}</div>;
"#;

        let barrel = r#"
export { MyWrapper, MyOpaqueComponent } from './MyComponents';
"#;

        let consumer = r#"
import { Table, Tbody } from '@patternfly/react-table';
import { MyWrapper } from '@app/components/wrappers';

const App = () => (
    <Table>
        <MyWrapper>
            <Tbody />
        </MyWrapper>
    </Table>
);
"#;

        let files = &[
            ("src/app/components/wrappers/MyComponents.tsx", wrapper),
            ("src/app/components/wrappers/index.ts", barrel),
            ("src/app/pages/App.tsx", consumer),
        ];

        // Rule: Tbody must be inside Table (fire when NOT inside Table)
        let condition = ReferencedCondition {
            pattern: "^Tbody$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: Some("^Table$".to_string()),
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-table".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents =
            scan_with_tsconfig_project(files, "src/app/pages/App.tsx", tsconfig, &condition);

        // MyWrapper is transparent, imported via named re-export through barrel.
        // Tbody should see Table as parent.
        assert!(
            incidents.is_empty(),
            "notParent: ^Table$ should NOT fire — MyWrapper is transparent \
             (named re-export through barrel). Got {} incidents",
            incidents.len()
        );
    }

    // ── Cross-file function reference resolution tests ───────────────────

    #[test]
    fn test_cross_file_fn_ref_prop_resolves_parent() {
        // toggle function is defined in a separate file and imported.
        // When used as <Select toggle={toggle}>, MenuToggle inside toggle's
        // body should see parentName=Select.
        let toggle_file = r#"
import { MenuToggle } from '@patternfly/react-core';

export const toggle = (toggleRef) => (
    <MenuToggle ref={toggleRef} onClick={onToggle}>
        Filter
    </MenuToggle>
);
"#;

        let consumer = r#"
import { Select } from '@patternfly/react-core';
import { toggle } from './components/toggle';

const App = () => (
    <Select toggle={toggle} isOpen={isOpen}>
        <div>options</div>
    </Select>
);
"#;

        let dir = std::env::temp_dir().join(format!(
            "scanner_fn_ref_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        let components_dir = src_dir.join("components");
        std::fs::create_dir_all(&components_dir).unwrap();

        std::fs::write(components_dir.join("toggle.tsx"), toggle_file).unwrap();
        let consumer_path = src_dir.join("App.tsx");
        std::fs::write(&consumer_path, consumer).unwrap();

        let condition = ReferencedCondition {
            pattern: "^MenuToggle$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: None,
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&consumer_path, &dir, &condition, &resolver_map, &mut cache, None)
                .unwrap();

        std::fs::remove_dir_all(&dir).ok();

        // Should find MenuToggle with parentName=Select via cross-file resolution
        let resolved = incidents.iter().find(|i| {
            i.variables.get("parentName") == Some(&serde_json::Value::String("Select".to_string()))
        });
        assert!(
            resolved.is_some(),
            "Should have MenuToggle with parentName=Select (cross-file fn ref resolution). \
             Got {} incidents: {:?}",
            incidents.len(),
            incidents
                .iter()
                .map(|i| i.variables.get("parentName"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_cross_file_fn_ref_through_barrel() {
        // toggle function exported through a barrel file (index.ts)
        let toggle_file = r#"
import { MenuToggle } from '@patternfly/react-core';

export const toggle = (toggleRef) => (
    <MenuToggle ref={toggleRef}>Filter</MenuToggle>
);
"#;

        let barrel = r#"
export { toggle } from './toggle';
"#;

        let consumer = r#"
import { Select } from '@patternfly/react-core';
import { toggle } from './components';

const App = () => (
    <Select toggle={toggle} isOpen={isOpen}>
        <div>options</div>
    </Select>
);
"#;

        let dir = std::env::temp_dir().join(format!(
            "scanner_fn_ref_barrel_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = dir.join("src");
        let components_dir = src_dir.join("components");
        std::fs::create_dir_all(&components_dir).unwrap();

        std::fs::write(components_dir.join("toggle.tsx"), toggle_file).unwrap();
        std::fs::write(components_dir.join("index.ts"), barrel).unwrap();
        let consumer_path = src_dir.join("App.tsx");
        std::fs::write(&consumer_path, consumer).unwrap();

        let condition = ReferencedCondition {
            pattern: "^MenuToggle$".to_string(),
            location: Some(ReferenceLocation::JsxComponent),
            component: None,
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: None,
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let resolver_map = crate::resolve::create_resolver_map(&dir, 3);
        let mut cache = crate::transparency::TransparencyCache::new();
        let (incidents, _) =
            scan_file_referenced(&consumer_path, &dir, &condition, &resolver_map, &mut cache, None)
                .unwrap();

        std::fs::remove_dir_all(&dir).ok();

        let resolved = incidents.iter().find(|i| {
            i.variables.get("parentName") == Some(&serde_json::Value::String("Select".to_string()))
        });
        assert!(
            resolved.is_some(),
            "Should resolve MenuToggle parentName=Select through barrel re-export. \
             Got {} incidents: {:?}",
            incidents.len(),
            incidents
                .iter()
                .map(|i| i.variables.get("parentName"))
                .collect::<Vec<_>>()
        );
    }

    // ── matches_package tests ────────────────────────────────────────────

    #[test]
    fn test_matches_package_exact_match() {
        assert!(matches_package(
            "@patternfly/react-tokens",
            "@patternfly/react-tokens"
        ));
    }

    #[test]
    fn test_matches_package_deep_path_dist_js() {
        assert!(matches_package(
            "@patternfly/react-tokens/dist/js/global_palette_black_500",
            "@patternfly/react-tokens"
        ));
    }

    #[test]
    fn test_matches_package_deep_path_dist_esm() {
        assert!(matches_package(
            "@patternfly/react-tokens/dist/esm/global_palette_black_500",
            "@patternfly/react-tokens"
        ));
    }

    #[test]
    fn test_matches_package_deep_path_lib() {
        assert!(matches_package(
            "@patternfly/react-icons/lib/esm/icons/TrashIcon",
            "@patternfly/react-icons"
        ));
    }

    #[test]
    fn test_matches_package_does_not_match_subpackage_deprecated() {
        assert!(!matches_package(
            "@patternfly/react-core/deprecated",
            "@patternfly/react-core"
        ));
    }

    #[test]
    fn test_matches_package_does_not_match_subpackage_next() {
        assert!(!matches_package(
            "@patternfly/react-core/next",
            "@patternfly/react-core"
        ));
    }

    #[test]
    fn test_matches_package_does_not_match_different_package() {
        assert!(!matches_package(
            "@patternfly/react-tokens-extra/dist/js/foo",
            "@patternfly/react-tokens"
        ));
    }

    #[test]
    fn test_matches_package_does_not_match_prefix_without_slash() {
        assert!(!matches_package(
            "@patternfly/react-core-internal",
            "@patternfly/react-core"
        ));
    }

    #[test]
    fn test_matches_package_deprecated_exact_match() {
        // When the rule explicitly targets the deprecated subpackage,
        // it should match exactly.
        assert!(matches_package(
            "@patternfly/react-core/deprecated",
            "@patternfly/react-core/deprecated"
        ));
    }

    // ── Cross-file spread resolution tests ───────────────────────────────

    #[test]
    fn test_cross_file_spread_identifier_object() {
        // Import an object from another file and spread it onto a JSX element.
        // The scanner should follow the import and extract property names.
        let utils = r#"
export const modalProps = { actions: [], title: "Hello" };
"#;
        let consumer = r#"
import { Modal } from '@patternfly/react-core';
import { modalProps } from './utils';
const el = <Modal {...modalProps}>content</Modal>;
"#;

        let tsconfig = r#"{ "compilerOptions": { "baseUrl": "." } }"#;
        let files = &[("src/utils.ts", utils), ("src/App.tsx", consumer)];

        let condition = ReferencedCondition {
            pattern: "^actions$".to_string(),
            location: Some(ReferenceLocation::JsxProp),
            component: Some("^Modal$".to_string()),
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents = scan_with_tsconfig_project(files, "src/App.tsx", tsconfig, &condition);
        assert_eq!(
            incidents.len(),
            1,
            "Should detect 'actions' from cross-file identifier spread"
        );
        assert_eq!(
            incidents[0].variables.get("spreadSource"),
            Some(&serde_json::Value::String("true".to_string()))
        );
    }

    #[test]
    fn test_cross_file_spread_fn_call() {
        // Import a function from another file that returns an object,
        // and spread its return value onto a JSX element.
        let utils = r#"
export const getModalProps = () => ({ actions: [], title: "Hello" });
"#;
        let consumer = r#"
import { Modal } from '@patternfly/react-core';
import { getModalProps } from './utils';
const el = <Modal {...getModalProps()}>content</Modal>;
"#;

        let tsconfig = r#"{ "compilerOptions": { "baseUrl": "." } }"#;
        let files = &[("src/utils.ts", utils), ("src/App.tsx", consumer)];

        let condition = ReferencedCondition {
            pattern: "^actions$".to_string(),
            location: Some(ReferenceLocation::JsxProp),
            component: Some("^Modal$".to_string()),
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents = scan_with_tsconfig_project(files, "src/App.tsx", tsconfig, &condition);
        assert_eq!(
            incidents.len(),
            1,
            "Should detect 'actions' from cross-file function call spread"
        );
    }

    #[test]
    fn test_cross_file_spread_through_barrel() {
        // Object re-exported through a barrel file (index.ts)
        let utils = r#"
export const modalProps = { actions: [], title: "Hello" };
"#;
        let barrel = r#"
export { modalProps } from './utils';
"#;
        let consumer = r#"
import { Modal } from '@patternfly/react-core';
import { modalProps } from './shared';
const el = <Modal {...modalProps}>content</Modal>;
"#;

        let tsconfig = r#"{ "compilerOptions": { "baseUrl": "." } }"#;
        let files = &[
            ("src/shared/utils.ts", utils),
            ("src/shared/index.ts", barrel),
            ("src/App.tsx", consumer),
        ];

        let condition = ReferencedCondition {
            pattern: "^actions$".to_string(),
            location: Some(ReferenceLocation::JsxProp),
            component: Some("^Modal$".to_string()),
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents = scan_with_tsconfig_project(files, "src/App.tsx", tsconfig, &condition);
        assert_eq!(
            incidents.len(),
            1,
            "Should detect 'actions' from spread through barrel re-export"
        );
    }

    #[test]
    fn test_cross_file_spread_fn_declaration() {
        // Cross-file function declaration (not arrow) that returns an object
        let utils = r#"
export function getModalProps() {
    return { actions: [], description: "text" };
}
"#;
        let consumer = r#"
import { Modal } from '@patternfly/react-core';
import { getModalProps } from './utils';
const el = <Modal {...getModalProps()}>content</Modal>;
"#;

        let tsconfig = r#"{ "compilerOptions": { "baseUrl": "." } }"#;
        let files = &[("src/utils.ts", utils), ("src/App.tsx", consumer)];

        let condition = ReferencedCondition {
            pattern: "^actions$".to_string(),
            location: Some(ReferenceLocation::JsxProp),
            component: Some("^Modal$".to_string()),
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: Some("@patternfly/react-core".to_string()),
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents = scan_with_tsconfig_project(files, "src/App.tsx", tsconfig, &condition);
        assert_eq!(
            incidents.len(),
            1,
            "Should detect 'actions' from cross-file function declaration return"
        );
    }

    #[test]
    fn test_cross_file_spread_node_modules_skipped() {
        // Identifiers imported from node_modules should NOT be followed
        // (we don't parse library source code)
        let consumer = r#"
import { Modal } from '@patternfly/react-core';
import { someProps } from 'some-library';
const el = <Modal {...someProps}>content</Modal>;
"#;

        let tsconfig = r#"{ "compilerOptions": { "baseUrl": "." } }"#;
        let files = &[("src/App.tsx", consumer)];

        let condition = ReferencedCondition {
            pattern: "^actions$".to_string(),
            location: Some(ReferenceLocation::JsxProp),
            component: None,
            parent: None,
            not_parent: None,
            parent_from: None,
            value: None,
            from: None,
            file_pattern: None,
            child: None,
            not_child: None,
            requires_child: None,
        };

        let incidents = scan_with_tsconfig_project(files, "src/App.tsx", tsconfig, &condition);
        assert!(
            incidents.is_empty(),
            "Should not produce incidents for node_modules spread identifiers"
        );
    }
}
