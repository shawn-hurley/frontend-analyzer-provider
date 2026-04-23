//! Evaluate a rule condition against the project.
//!
//! Dispatches to the appropriate scanner (js-scanner or css-scanner)
//! based on the capability name.

use crate::proto::{IncidentContext, Location, Position, ProviderEvaluateResponse};
use anyhow::Result;
use ast_index_react::types::ReactProjectIndex;
use frontend_core::capabilities::ProviderCondition;
use frontend_core::incident::Incident;
use frontend_js_scanner::scanner::ParseError;
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;

/// Result of evaluating a condition, including any files that could not be parsed.
pub struct EvaluationResult {
    /// The gRPC response with matched incidents.
    pub response: ProviderEvaluateResponse,
    /// Files that could not be parsed (syntax errors, broken imports, etc.).
    /// Deduplicated by file path so each file is reported at most once.
    pub parse_errors: Vec<ParseError>,
}

/// Evaluate a single condition against the project.
///
/// When called via kantra/analyzer-lsp, `condition_yaml` contains a wrapper
/// with `tags`, `template`, `ruleID`, `depLabelSelector` and the actual
/// condition nested under the capability key. We extract the nested condition.
///
/// Returns both the matched incidents and any parse errors encountered.
/// Parse errors indicate files that could not be analyzed due to syntax
/// problems (e.g., broken imports from a previous migration run).
pub fn evaluate_condition(
    root: &Path,
    capability: &str,
    condition_yaml: &str,
    react_index: Option<&ReactProjectIndex>,
) -> Result<EvaluationResult> {
    // Try to extract the nested condition from the kantra wrapper format.
    // The wrapper has the condition under a key matching the capability name.
    let effective_yaml = extract_nested_condition(capability, condition_yaml)
        .unwrap_or_else(|| condition_yaml.to_string());

    let condition = ProviderCondition::parse(capability, &effective_yaml)?;

    let mut all_incidents = Vec::new();
    let mut parse_errors = Vec::new();
    // Track which files we've already reported errors for to avoid duplicates.
    let mut errored_files: HashSet<std::path::PathBuf> = HashSet::new();

    match condition {
        ProviderCondition::Referenced(cond) => {
            let files =
                frontend_js_scanner::scanner::collect_files(root, cond.file_pattern.as_deref())?;

            // Create a resolver map once for the entire rule evaluation.
            let resolver_map = frontend_js_scanner::resolve::create_resolver_map(root, 3);

            if let Some(react_idx) = react_index {
                // Fast path: use the React project index for parallel scanning.
                // All cross-file data comes from the pre-built index, so no
                // mutable state is needed — files can be scanned concurrently.
                let file_index = frontend_js_scanner::index_lookup::FileIndex::new(react_idx);
                let (incidents, errors) =
                    frontend_js_scanner::scanner::scan_files_parallel(
                        &files,
                        root,
                        &cond,
                        &resolver_map,
                        &file_index,
                    )?;
                all_incidents.extend(incidents);
                for err in errors {
                    if errored_files.insert(err.file_path.clone()) {
                        parse_errors.push(err);
                    }
                }
            } else {
                // Fallback: sequential scanning with mutable transparency cache.
                let mut transparency_cache =
                    frontend_js_scanner::transparency::TransparencyCache::new();
                for file in files {
                    let (incidents, parse_error) =
                        frontend_js_scanner::scanner::scan_file_referenced(
                            &file,
                            root,
                            &cond,
                            &resolver_map,
                            &mut transparency_cache,
                            None,
                        )?;
                    all_incidents.extend(incidents);
                    if let Some(err) = parse_error {
                        if errored_files.insert(err.file_path.clone()) {
                            parse_errors.push(err);
                        }
                    }
                }
            }
        }
        ProviderCondition::CssClass(cond) => {
            let pattern = Regex::new(&cond.pattern)?;

            // Scan CSS/SCSS files
            let css_files = frontend_css_scanner::scanner::collect_css_files(
                root,
                cond.file_pattern.as_deref(),
            )?;
            for file in &css_files {
                let result =
                    frontend_css_scanner::scanner::scan_css_file_classes(file, root, &pattern)?;
                all_incidents.extend(result);
            }

            // Also scan JS/TS files for className usage
            let js_files =
                frontend_js_scanner::scanner::collect_files(root, cond.file_pattern.as_deref())?;
            for file in &js_files {
                let (incidents, parse_error) =
                    frontend_js_scanner::scanner::scan_file_classnames(file, root, &pattern)?;
                all_incidents.extend(incidents);
                if let Some(err) = parse_error {
                    if errored_files.insert(err.file_path.clone()) {
                        parse_errors.push(err);
                    }
                }
            }
        }
        ProviderCondition::CssVar(cond) => {
            let pattern = Regex::new(&cond.pattern)?;

            // Scan CSS files
            let css_files = frontend_css_scanner::scanner::collect_css_files(
                root,
                cond.file_pattern.as_deref(),
            )?;
            for file in &css_files {
                let result =
                    frontend_css_scanner::scanner::scan_css_file_vars(file, root, &pattern)?;
                all_incidents.extend(result);
            }

            // Also scan JS/TS files for CSS var references
            let js_files =
                frontend_js_scanner::scanner::collect_files(root, cond.file_pattern.as_deref())?;
            for file in &js_files {
                let (incidents, parse_error) =
                    frontend_js_scanner::scanner::scan_file_css_vars(file, root, &pattern)?;
                all_incidents.extend(incidents);
                if let Some(err) = parse_error {
                    if errored_files.insert(err.file_path.clone()) {
                        parse_errors.push(err);
                    }
                }
            }
        }
        ProviderCondition::Dependency(cond) => {
            all_incidents.extend(frontend_js_scanner::dependency::check_dependencies(
                root, &cond,
            )?);
        }
    };

    let incident_contexts: Vec<IncidentContext> =
        all_incidents.iter().map(incident_to_proto).collect();
    let matched = !incident_contexts.is_empty();

    Ok(EvaluationResult {
        response: ProviderEvaluateResponse {
            matched,
            incident_contexts,
            template_context: None,
        },
        parse_errors,
    })
}

/// Convert an internal Incident to a gRPC IncidentContext.
fn incident_to_proto(incident: &Incident) -> IncidentContext {
    let variables = if incident.variables.is_empty() {
        None
    } else {
        let fields = incident
            .variables
            .iter()
            .map(|(k, v)| {
                let prost_value = json_to_prost_value(v);
                (k.clone(), prost_value)
            })
            .collect();
        Some(prost_types::Struct { fields })
    };

    IncidentContext {
        file_uri: incident.file_uri.clone(),
        effort: incident.effort,
        code_location: incident.code_location.as_ref().map(|loc| Location {
            start_position: Some(Position {
                line: loc.start.line as f64,
                character: loc.start.character as f64,
            }),
            end_position: Some(Position {
                line: loc.end.line as f64,
                character: loc.end.character as f64,
            }),
        }),
        line_number: incident.line_number.map(|n| n as i64),
        variables,
        links: incident
            .links
            .iter()
            .map(|l| crate::proto::ExternalLink {
                url: l.url.clone(),
                title: l.title.clone(),
            })
            .collect(),
        is_dependency_incident: incident.is_dependency_incident,
    }
}

/// Extract nested condition from kantra's wrapper format.
///
/// Kantra sends condition_info as:
/// ```yaml
/// tags: {}
/// template: {}
/// ruleID: pfv6-some-rule
/// depLabelSelector: '...'
/// referenced:         # <-- capability key
///   pattern: ^Foo$
///   location: IMPORT
/// ```
///
/// We extract the YAML under the capability key and return it as a standalone
/// condition string.
fn extract_nested_condition(capability: &str, condition_yaml: &str) -> Option<String> {
    let parsed: serde_json::Value = yaml_serde::from_str(condition_yaml).ok()?;
    let nested = parsed.get(capability)?;
    yaml_serde::to_string(nested).ok()
}

fn json_to_prost_value(v: &serde_json::Value) -> prost_types::Value {
    match v {
        serde_json::Value::String(s) => prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s.clone())),
        },
        serde_json::Value::Number(n) => prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(
                n.as_f64().unwrap_or_default(),
            )),
        },
        serde_json::Value::Bool(b) => prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(*b)),
        },
        serde_json::Value::Null => prost_types::Value {
            kind: Some(prost_types::value::Kind::NullValue(0)),
        },
        _ => prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(v.to_string())),
        },
    }
}
