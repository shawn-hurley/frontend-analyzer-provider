//! File-level scanning utilities.
//!
//! Provides file discovery (`collect_files`) and per-file CSS class / CSS var
//! scanning. The main JS/TS/JSX referenced-symbol evaluation has moved to
//! [`crate::query_eval`], which queries the pre-built React project index.

use anyhow::Result;
use frontend_core::incident::{extract_code_snip, Incident, Location, Position};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use regex::Regex;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

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
        assert_eq!(loc.start.line, 0);
        assert_eq!(loc.start.character, 0);
    }

    #[test]
    fn test_make_incident_second_line() {
        let source = "line one\nimport { X } from 'y';";
        let incident = make_incident(source, "file:///test.tsx", 9, 30);
        assert_eq!(incident.line_number, Some(2));
        let loc = incident.code_location.unwrap();
        assert_eq!(loc.start.line, 1);
        assert_eq!(loc.start.character, 0);
    }

    #[test]
    fn test_make_incident_column_calculation() {
        let source = "  const x = 1;";
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
        assert!(st.is_jsx());
    }

    #[test]
    fn test_source_type_js_with_require() {
        let st = source_type_for_file(Path::new("app.js"), "const foo = require('bar');");
        assert!(st.is_jsx());
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
