//! Evaluate a rule condition against the project.
//!
//! Dispatches to the appropriate scanner (js-scanner or css-scanner)
//! based on the capability name.

use crate::proto::{IncidentContext, Location, Position, ProviderEvaluateResponse};
use anyhow::Result;
use frontend_core::capabilities::ProviderCondition;
use frontend_core::incident::Incident;
use regex::Regex;
use std::path::Path;

/// Evaluate a single condition against the project.
///
/// When called via kantra/analyzer-lsp, `condition_yaml` contains a wrapper
/// with `tags`, `template`, `ruleID`, `depLabelSelector` and the actual
/// condition nested under the capability key. We extract the nested condition.
pub fn evaluate_condition(
    root: &Path,
    capability: &str,
    condition_yaml: &str,
) -> Result<ProviderEvaluateResponse> {
    // Try to extract the nested condition from the kantra wrapper format.
    // The wrapper has the condition under a key matching the capability name.
    let effective_yaml = extract_nested_condition(capability, condition_yaml)
        .unwrap_or_else(|| condition_yaml.to_string());

    let condition = ProviderCondition::parse(capability, &effective_yaml)?;
    let incidents = match condition {
        ProviderCondition::Referenced(cond) => {
            let files =
                frontend_js_scanner::scanner::collect_files(root, cond.file_pattern.as_deref())?;
            let mut all_incidents = Vec::new();
            for file in files {
                let result =
                    frontend_js_scanner::scanner::scan_file_referenced(&file, root, &cond)?;
                all_incidents.extend(result);
            }
            all_incidents
        }
        ProviderCondition::CssClass(cond) => {
            let pattern = Regex::new(&cond.pattern)?;

            // Scan CSS/SCSS files
            let css_files = frontend_css_scanner::scanner::collect_css_files(
                root,
                cond.file_pattern.as_deref(),
            )?;
            let mut all_incidents = Vec::new();
            for file in &css_files {
                let result =
                    frontend_css_scanner::scanner::scan_css_file_classes(file, root, &pattern)?;
                all_incidents.extend(result);
            }

            // Also scan JS/TS files for className usage
            let js_files =
                frontend_js_scanner::scanner::collect_files(root, cond.file_pattern.as_deref())?;
            for file in &js_files {
                let result =
                    frontend_js_scanner::scanner::scan_file_classnames(file, root, &pattern)?;
                all_incidents.extend(result);
            }

            all_incidents
        }
        ProviderCondition::CssVar(cond) => {
            let pattern = Regex::new(&cond.pattern)?;

            // Scan CSS files
            let css_files = frontend_css_scanner::scanner::collect_css_files(
                root,
                cond.file_pattern.as_deref(),
            )?;
            let mut all_incidents = Vec::new();
            for file in &css_files {
                let result =
                    frontend_css_scanner::scanner::scan_css_file_vars(file, root, &pattern)?;
                all_incidents.extend(result);
            }

            // Also scan JS/TS files for CSS var references
            let js_files =
                frontend_js_scanner::scanner::collect_files(root, cond.file_pattern.as_deref())?;
            for file in &js_files {
                let result =
                    frontend_js_scanner::scanner::scan_file_css_vars(file, root, &pattern)?;
                all_incidents.extend(result);
            }

            all_incidents
        }
        ProviderCondition::Dependency(cond) => {
            frontend_js_scanner::dependency::check_dependencies(root, &cond)?
        }
    };

    let incident_contexts: Vec<IncidentContext> = incidents.iter().map(incident_to_proto).collect();
    let matched = !incident_contexts.is_empty();

    Ok(ProviderEvaluateResponse {
        matched,
        incident_contexts,
        template_context: None,
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
