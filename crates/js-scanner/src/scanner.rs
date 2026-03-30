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

/// Result of scanning: a list of incidents.
pub type ScanResult = Vec<Incident>;

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
pub fn scan_file_referenced(
    file_path: &Path,
    root: &Path,
    condition: &ReferencedCondition,
) -> Result<ScanResult> {
    let source = std::fs::read_to_string(file_path)?;
    let source_type = source_type_for_file(file_path, &source);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    if ret.panicked {
        tracing::warn!("Parser panicked on {}", file_path.display());
        return Ok(Vec::new());
    }

    let pattern_re = Regex::new(&condition.pattern)?;
    let file_uri = path_to_uri(file_path, root);
    let mut incidents = Vec::new();

    let location = condition.location.as_ref();

    // Build import map so JSX scanning can resolve components to their
    // import source (e.g., Button → @patternfly/react-core).
    let import_map = crate::imports::build_import_map(&ret.program);

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
            Some(ReferenceLocation::JsxComponent) | Some(ReferenceLocation::JsxProp) | None => {
                incidents.extend(crate::jsx::scan_jsx(
                    stmt,
                    &source,
                    &pattern_re,
                    &file_uri,
                    location,
                    &import_map,
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

    // Filter by prop value if specified
    if let Some(value_pattern) = &condition.value {
        let value_re = Regex::new(value_pattern)?;
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(val)) = inc.variables.get("propValue") {
                value_re.is_match(val)
            } else {
                // No propValue = boolean prop or no value, filter out
                false
            }
        });
    }

    // Filter by import source path if specified.
    // Works for both import incidents (module from import statement) and
    // JSX incidents (module resolved from the file's import map).
    if let Some(from_pattern) = &condition.from {
        let from_re = Regex::new(from_pattern)?;
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(module)) = inc.variables.get("module") {
                from_re.is_match(module)
            } else {
                // No module = component not found in imports (e.g., locally
                // defined or HTML element). Keep it to avoid false negatives.
                true
            }
        });
    }

    // Filter by parent import source path if specified.
    // Matches the parent JSX component's import source, resolved from the
    // file's import map (e.g., parentFrom: "@patternfly/react-core").
    if let Some(parent_from_pattern) = &condition.parent_from {
        let parent_from_re = Regex::new(parent_from_pattern)?;
        incidents.retain(|inc| {
            if let Some(serde_json::Value::String(module)) = inc.variables.get("parentFrom") {
                parent_from_re.is_match(module)
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

    Ok(incidents)
}

/// Scan a single file for CSS class name references in JS/TS (className attributes, etc.).
pub fn scan_file_classnames(file_path: &Path, root: &Path, pattern: &Regex) -> Result<ScanResult> {
    let source = std::fs::read_to_string(file_path)?;
    let source_type = source_type_for_file(file_path, &source);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    if ret.panicked {
        tracing::warn!("Parser panicked on {}", file_path.display());
        return Ok(Vec::new());
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

    Ok(incidents)
}

/// Scan a single file for CSS variable references in JS/TS.
pub fn scan_file_css_vars(file_path: &Path, root: &Path, pattern: &Regex) -> Result<ScanResult> {
    let source = std::fs::read_to_string(file_path)?;
    let source_type = source_type_for_file(file_path, &source);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, source_type).parse();

    if ret.panicked {
        tracing::warn!("Parser panicked on {}", file_path.display());
        return Ok(Vec::new());
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

    Ok(incidents)
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
}
